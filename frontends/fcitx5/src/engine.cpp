#include "engine.h"

#include <algorithm>
#include <cstdlib>
#include <string>

#include <fcitx/action.h>
#include <fcitx/candidatelist.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputcontextmanager.h>
#include <fcitx/instance.h>
#include <fcitx/inputpanel.h>
#include <fcitx/statusarea.h>
#include <fcitx/text.h>
#include <fcitx/userinterface.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/keysym.h>

namespace wetype {
namespace {

/// Candidates per page. Small on purpose: five is what a horizontal candidate
/// bar comfortably shows with labels.
constexpr uint32_t kPageSize = 5;

/// Same resolution order as the daemon, so both sides agree without config.
///
/// Empty variables count as unset: an empty `XDG_RUNTIME_DIR` would otherwise
/// resolve to `/wetype.sock`, and an empty `HOME` to `/.local/...`.
std::string socketPath() {
    if (const char *env = std::getenv("WETYPE_SOCKET"); env && *env) {
        return env;
    }
    if (const char *runtime = std::getenv("XDG_RUNTIME_DIR");
        runtime && *runtime) {
        return std::string(runtime) + "/wetype.sock";
    }
    if (const char *home = std::getenv("HOME"); home && *home) {
        return std::string(home) + "/.local/state/wetype/wetype.sock";
    }
    return "/tmp/wetype.sock";
}

std::string asString(const uint8_t *data, uintptr_t len) {
    if (!data || len == 0) {
        return {};
    }
    return std::string(reinterpret_cast<const char *>(data), len);
}

/// Candidate word that routes a click back into the engine.
///
/// `DisplayOnlyCandidateWord` would be simpler, but it cannot be selected, so
/// clicking a candidate would do nothing.
class SelectCandidateWord : public fcitx::CandidateWord {
public:
    SelectCandidateWord(fcitx::Text text, uint32_t index, Engine *engine)
        : CandidateWord(std::move(text)), index_(index), engine_(engine) {}

    void select(fcitx::InputContext *ic) const override {
        engine_->selectByIndex(ic, index_);
    }

private:
    uint32_t index_;
    Engine *engine_;
};

} // namespace

// ------------------------------------------------------------------- state

State::~State() {
    if (session_ != 0 && handle_) {
        wetype_close_session(handle_.get(), session_);
    }
}

bool State::ensureSession(fcitx::InputContext &) {
    const uint64_t connection = wetype_generation(handle_.get());
    if (session_ != 0 && connection_ != connection) {
        // The daemon restarted, and every session died with it. Session ids are
        // not reused across restarts, so the old one must not be kept even if
        // the number happens to exist again.
        session_ = 0;
    }
    if (session_ != 0) {
        return true;
    }
    // NOTE: app name is empty for now. fcitx5 can report the program name via
    // InputContext::program(); wiring it through needs a test on a real desktop
    // (the engine uses it for per-app scene decisions).
    uint64_t session = 0;
    if (wetype_new_session(handle_.get(), 0, "", &session) != 0) {
        return false;
    }
    session_ = session;
    connection_ = connection;
    return true;
}

fcitx::InputContextProperty *StateFactory::create(fcitx::InputContext &) {
    // The handle is shared: states may outlive the Engine object during
    // shutdown, and the connection must stay valid until the last one is gone.
    return new State(handle_);
}

// ------------------------------------------------------------------ engine

Engine::Engine(fcitx::AddonManager *manager)
    : handle_(wetype_connect(socketPath().c_str()), HandleDeleter{}),
      factory_(handle_) {
    // Registering the factory is mandatory: InputContext::propertyFor()
    // asserts that the factory belongs to this instance's manager. Skipping it
    // aborts fcitx5 the first time any input context is reset.
    if (manager && manager->instance()) {
        instance_ = manager->instance();
        if (!instance_->inputContextManager().registerProperty("wetypeState",
                                                               &factory_)) {
            fprintf(stderr, "wetype: cannot register input context property\n");
        }
    }
    // A missing daemon is not fatal: calls fail fast and the addon simply does
    // not intercept keys until it is restarted, instead of breaking input.
    if (!handle_) {
        fprintf(stderr, "wetype: cannot create client handle\n");
        return;
    }
    // Bump-check the daemon once. A daemon built against an older protocol
    // decodes and re-encodes every reply, silently dropping fields it does not
    // know - which is exactly how `raw` went missing once.
    reloadConfig();
    updatePunctuationLabel();
    if (instance_) {
        punctuationAction_.registerAction("wetype-punctuation",
                                          &instance_->userInterfaceManager());
        // The status-area entry is the second switch for the same option: users
        // flip Chinese punctuation far more often than they open the config
        // page, and this is where every other IME puts it.
        punctuationAction_.connect<fcitx::SimpleAction::Activated>(
            [this](fcitx::InputContext *ic) {
                config_.chinesePunctuation.setValue(
                    !config_.chinesePunctuation.value());
                rebuildPunctuation();
                updatePunctuationLabel();
                // Persist immediately, otherwise the choice is lost the next
                // time fcitx5 reloads its addon configuration.
                save();
                if (ic) {
                    ic->updateUserInterface(
                        fcitx::UserInterfaceComponent::StatusArea);
                }
            });
    }
    if (wetype_ping(handle_.get(), wetype_protocol_version()) != 0) {
        fprintf(stderr, "wetype: daemon check failed: %s\n",
                wetype_last_error(handle_.get()));
    }
}

bool Engine::isSwitchKey(const fcitx::Key &key) const {
    const auto sym = key.sym();
    const auto mode = config_.languageSwitch.value();
    const bool shiftEnabled =
        mode == LanguageSwitch::Shift || mode == LanguageSwitch::Both;
    const bool ctrlEnabled =
        mode == LanguageSwitch::Ctrl || mode == LanguageSwitch::Both;
    if (sym == FcitxKey_Shift_L || sym == FcitxKey_Shift_R) {
        return shiftEnabled;
    }
    if (sym == FcitxKey_Control_L || sym == FcitxKey_Control_R) {
        return ctrlEnabled;
    }
    return false;
}

bool Engine::daemonAvailable() const {
    return handle_ && wetype_connected(handle_.get());
}

std::string Engine::modeLabel(bool english) const {
    // Only the Chinese label carries the warning: English mode is a plain
    // pass-through and keeps working without the daemon, so flagging it there
    // would blame a service that mode does not use.
    if (english) {
        return "abc";
    }
    return daemonAvailable() ? "中" : "中（服务缺失）";
}

void Engine::warnServiceMissing(fcitx::InputContext *ic) {
    // Once per outage: a popup on every keystroke would be worse than silence.
    // `daemonAvailable()` filters out the case where the socket is fine but one
    // session creation failed (for example the per-daemon session limit).
    if (serviceWarned_ || !instance_ || daemonAvailable()) {
        return;
    }
    serviceWarned_ = true;
    instance_->showCustomInputMethodInformation(ic, "中（服务缺失）");
}

void Engine::setConfig(const fcitx::RawConfig &raw) {
    config_.load(raw, true);
    rebuildPunctuation();
    // The status entry mirrors the option, so it has to follow a config-page
    // edit as well.
    updatePunctuationLabel();
}

void Engine::reloadConfig() {
    fcitx::readAsIni(config_, "conf/wetype.conf");
    rebuildPunctuation();
    updatePunctuationLabel();
}

void Engine::save() { fcitx::safeSaveAsIni(config_, "conf/wetype.conf"); }

Engine::~Engine() {
    // The factory must not outlive its registration.
    factory_.unregister();
}

void Engine::keyEvent(const fcitx::InputMethodEntry &, fcitx::KeyEvent &event) {
    if (!handle_) {
        return;
    }
    auto *ic = event.inputContext();
    auto *state = ic->propertyFor(&factory_);
    const auto key = event.key();

    // A daemon restart invalidates every session. Notice it before routing the
    // key: otherwise the stale composition would be sent to a session that no
    // longer exists and the error would only surface one call later.
    if (state->session() != 0 &&
        state->connection() != wetype_generation(handle_.get())) {
        state->dropSession();
        clearUI(ic, state);
    }

    // --- switch key, press/release pair -------------------------------------
    //
    // A bare modifier must only switch when it was pressed and released on its
    // own: otherwise every Ctrl+C would toggle the language. The press arms,
    // any other key disarms, the release acts. Neither event is consumed, so
    // the application still sees a normal modifier press.
    if (isSwitchKey(key)) {
        if (event.isRelease()) {
            if (state->switchArmed_) {
                state->switchArmed_ = false;
                // Committing the raw composition is what lets the user type the
                // literal letters they just entered.
                // Prefer our copy of the raw composition; fall back to what
                // fcitx is actually displaying, so a stale/empty state can
                // never silently swallow what the user typed.
                std::string text;
                if (!state->raw_.empty()) {
                    text = state->raw_;
                } else if (!state->preedit_.empty()) {
                    text = state->preedit_;
                } else {
                    text = ic->inputPanel().preedit().toStringForCommit();
                }
                if (!text.empty()) {
                    ic->commitString(text);
                }
                resetSession(ic, state);
                state->english_ = !state->english_;
                // A small popup with the new mode, the way every Chinese IME
                // reports the switch.
                if (instance_) {
                    instance_->showCustomInputMethodInformation(
                        ic, modeLabel(state->english_));
                }
                // filter(), not filterAndAccept(): the physical press/release
                // must still reach the client, otherwise it believes the
                // modifier is held down (a stuck Ctrl makes the wheel zoom).
                event.filter();
            }
        } else {
            state->switchArmed_ = true;
            event.filter();
        }
        return;
    }

    if (event.isRelease()) {
        return;
    }
    state->switchArmed_ = false;

    // --- anything carrying a real modifier belongs to the application -------
    //
    // SimpleMask is Ctrl|Alt|Shift|Super|Super2|Hyper|Meta; Shift is allowed
    // because it is how capitals are typed.
    // KeyState is an enum class without bitwise operators, so work in the
    // underlying integer type. SimpleMask = Ctrl|Alt|Shift|Super|Super2|Hyper|
    // Meta; only Shift may accompany a composing key.
    constexpr uint32_t kNonShiftMask =
        static_cast<uint32_t>(fcitx::KeyState::SimpleMask) &
        ~static_cast<uint32_t>(fcitx::KeyState::Shift);
    // rawKey() is what the frontend delivered; key() is normalised and may drop
    // modifier bits. Check both so a shortcut never slips through.
    const uint32_t raw_states = static_cast<uint32_t>(event.rawKey().states());
    const uint32_t key_states = static_cast<uint32_t>(key.states());
    if ((raw_states | key_states) & kNonShiftMask) {
        return;
    }

    const auto sym = key.sym();
    const bool letter = (sym >= FcitxKey_a && sym <= FcitxKey_z) ||
                        (sym >= FcitxKey_A && sym <= FcitxKey_Z);

    // English mode: do not intercept letters at all, the application types them.
    if (letter && state->english_) {
        return;
    }

    if (sym == FcitxKey_Escape) {
        // Only a live composition owns Escape. Gating on `session() != 0` once
        // swallowed Escape forever: the session is created on the first letter
        // and never released, so every later Escape in that window was eaten
        // even with nothing composing.
        if (!state->preedit_.empty()) {
            resetSession(ic, state);
            event.filterAndAccept();
        }
        return;
    }

    // Letters start (or extend) a composition.
    if (letter) {
        if (!state->ensureSession(*ic)) {
            warnServiceMissing(ic);
            return;
        }
        // A session exists again, so the daemon is reachable.
        serviceWarned_ = false;
        // The engine accepts uppercase and answers with capitalised candidates
        // (Nihao -> NiHao), so the letter is forwarded with the case the user
        // typed. Two sources have to be covered because frontends differ: the
        // keysym may already be shifted ('A'), or it may arrive as 'a' with the
        // Shift bit set on the raw key.
        const bool shifted =
            ((raw_states | key_states) &
             static_cast<uint32_t>(fcitx::KeyState::Shift)) != 0;
        uint8_t code = static_cast<uint8_t>(sym);
        if (shifted && sym >= FcitxKey_a && sym <= FcitxKey_z) {
            code = static_cast<uint8_t>(sym - FcitxKey_a + 'A');
        }
        if (sendKey(ic, state, code)) {
            event.filterAndAccept();
        }
        return;
    }

    // `'` is the pinyin syllable separator while a composition exists: `xi'an`
    // is two syllables, and the engine accepts the separator itself. Only
    // outside a composition is it a quote, which the punctuation block below
    // handles. Intercepting it here instead is what made `xi'an` impossible to
    // type - the key first committed 西 and then a quote.
    if (sym == FcitxKey_apostrophe && !state->preedit_.empty()) {
        if (sendKey(ic, state, static_cast<uint8_t>('\''))) {
            event.filterAndAccept();
        }
        return;
    }

    // Chinese punctuation: commit the current first candidate, then write the
    // symbol straight into the application. This is what WeType itself does,
    // and it behaves the same in every client (no preedit rendering involved).
    if (!state->english_ && config_.chinesePunctuation.value() && sym > 0x20 &&
        sym < 0x7f) {
        if (const auto it = punctuation_.find(static_cast<char>(sym));
            it != punctuation_.end()) {
            // Quotes alternate: opening the pair first, closing it next. The
            // parity only moves for a key that really produced a Chinese quote,
            // so an ASCII quote (mapping switched off) cannot desync it.
            std::string mapped = it->second;
            if (sym == FcitxKey_apostrophe || sym == FcitxKey_quotedbl) {
                const bool single = sym == FcitxKey_apostrophe;
                bool &open =
                    single ? state->openSingleQuote_ : state->openDoubleQuote_;
                mapped = single ? (open ? "‘" : "’") : (open ? "“" : "”");
                open = !open;
            }
            // Finish the composition in the SAME commit as the symbol: two
            // separate commits make clients reorder or drop the candidate.
            std::string prefix;
            if (!state->preedit_.empty()) {
                prefix = !state->firstCandidate_.empty()
                             ? state->firstCandidate_
                             : (!state->raw_.empty() ? state->raw_
                                                     : state->preedit_);
            }
            ic->commitString(prefix + mapped);
            if (!state->preedit_.empty()) {
                resetSession(ic, state);
            }
            event.filterAndAccept();
            return;
        }
    }

    if (state->session() == 0 || state->preedit_.empty()) {
        return; // nothing composing: leave the key alone
    }

    if (sym == FcitxKey_BackSpace) {
        if (sendBackspace(ic, state)) {
            event.filterAndAccept();
        }
        return;
    }

    // Enter and space always end the composition, taking either the raw pinyin
    // or the first candidate as configured. Doing it unconditionally also
    // covers the empty-candidate case: leaving the key to the application used
    // to insert a literal space while the preedit was still on screen.
    if (sym == FcitxKey_Return || sym == FcitxKey_KP_Enter ||
        sym == FcitxKey_space) {
        const CommitBehavior behavior = sym == FcitxKey_space
                                            ? config_.spaceBehavior.value()
                                            : config_.enterBehavior.value();
        finishComposition(ic, state, behavior == CommitBehavior::Raw);
        event.filterAndAccept();
        return;
    }

    // Paging. fcitx5 core does not page candidate lists for the engine, so the
    // engine has to do it.
    if ((sym == FcitxKey_Page_Down || sym == FcitxKey_Down ||
         sym == FcitxKey_Page_Up || sym == FcitxKey_Up) &&
        pageCandidates(ic, sym)) {
        event.filterAndAccept();
        return;
    }

    if (sym >= FcitxKey_1 && sym <= FcitxKey_9 && state->hasCandidates_) {
        // The digit is relative to the visible page; the engine wants a global
        // index, so the page offset has to be added exactly once - here.
        const uint32_t global =
            pageBase(ic) + static_cast<uint32_t>(sym - FcitxKey_1);
        if (selectCandidate(ic, state, global)) {
            event.filterAndAccept();
        }
        return;
    }

    // Anything else is not ours: leave the composition exactly as it is and let
    // the key reach the application. Clearing here is what made a bare Shift
    // "discard" the composition without committing anything.
}

void Engine::reset(const fcitx::InputMethodEntry &, fcitx::InputContextEvent &event) {
    auto *ic = event.inputContext();
    if (!ic) {
        return;
    }
    auto *state = ic->propertyFor(&factory_);
    // A reset ends whatever was happening, and that includes a bare modifier
    // that was pressed but not released yet. Without this the matching release
    // can still fire after the user left the context and toggle the mode.
    state->switchArmed_ = false;
    if (handle_ && state->session() != 0) {
        resetSession(ic, state);
    }
}

void Engine::adoptCandidates(State *state, const WetypeCandidates &candidates) {
    state->generation_ = candidates.generation;
    state->hasCandidates_ = candidates.items_len > 0;
    state->firstCandidate_.clear();
    if (candidates.items_len > 0) {
        const auto &first = candidates.items[0];
        state->firstCandidate_ = asString(first.text, first.text_len);
    }
}

bool Engine::sendKey(fcitx::InputContext *ic, State *state, uint8_t key) {
    WetypeCandidates candidates{};
    if (wetype_key(handle_.get(), state->session(), key, &candidates) != 0) {
        // Either the daemon is down or the session was invalidated by a
        // restart. Drop both the session and the composition: keeping the
        // preedit would make the next key append to text the engine no longer
        // has. The client reconnects on its own, so the next letter rebuilds
        // everything.
        state->dropSession();
        clearUI(ic, state);
        return false;
    }
    adoptCandidates(state, candidates);

    // The engine may decide to commit something on its own (punctuation).
    if (candidates.commit_len > 0) {
        ic->commitString(asString(candidates.commit, candidates.commit_len));
    }
    updateUI(ic, state, candidates);
    wetype_candidates_free(&candidates);
    return true;
}

bool Engine::sendBackspace(fcitx::InputContext *ic, State *state) {
    WetypeCandidates candidates{};
    if (wetype_backspace(handle_.get(), state->session(), 1, &candidates) != 0) {
        state->dropSession();
        clearUI(ic, state);
        return false;
    }
    adoptCandidates(state, candidates);
    if (candidates.commit_len > 0) {
        ic->commitString(asString(candidates.commit, candidates.commit_len));
    }
    updateUI(ic, state, candidates);
    wetype_candidates_free(&candidates);
    return true;
}

uint32_t Engine::pageBase(fcitx::InputContext *ic) {
    if (auto list = ic->inputPanel().candidateList()) {
        if (auto *pageable = list->toPageable()) {
            const int page = pageable->currentPage();
            if (page > 0) {
                return static_cast<uint32_t>(page) * kPageSize;
            }
        }
    }
    return 0;
}

bool Engine::selectCandidate(fcitx::InputContext *ic, State *state,
                             uint32_t global) {
    // `global` is already a list index: mouse clicks pass the index stored in
    // the candidate word, the keyboard adds the current page offset itself.
    // Adding the page here as well is what made every click past page one
    // select nothing (or the wrong word).
    WetypeSelectResult result{};
    if (wetype_select(handle_.get(), state->session(), state->generation_, global,
                      &result) != 0) {
        // A stale candidate generation is a normal race, and the session stays
        // usable, so this one does not drop the session - only the UI, which no
        // longer matches what the engine holds.
        clearUI(ic, state);
        return false;
    }
    if (result.commit_len > 0) {
        ic->commitString(asString(result.commit, result.commit_len));
    }
    if (result.finished) {
        // Whole-word selection: the composition ends here.
        clearUI(ic, state);
    } else {
        // Partial selection: `commit` went into the application, and the rest
        // of the pinyin is a normal composition again. Only that remainder is
        // shown - the committed text belongs to the application now. The engine
        // host rebuilt the session, so this snapshot is all we need.
        adoptCandidates(state, result.rest);
        updateUI(ic, state, result.rest);
    }
    wetype_select_result_free(&result);
    return true;
}

void Engine::rebuildPunctuation() {
    punctuation_.clear();
    const auto add = [this](bool enabled, char key, const char *value) {
        if (enabled) {
            punctuation_[key] = value;
        }
    };
    add(config_.symbolComma.value(), ',', "，");
    add(config_.symbolPeriod.value(), '.', "。");
    add(config_.symbolSemicolon.value(), ';', "；");
    add(config_.symbolApostrophe.value(), '\'', "‘");
    add(config_.symbolQuestion.value(), '?', "？");
    add(config_.symbolExclam.value(), '!', "！");
    add(config_.symbolColon.value(), ':', "：");
    add(config_.symbolQuote.value(), '"', "“");
    // Both halves share one switch.
    add(config_.symbolParen.value(), '(', "（");
    add(config_.symbolParen.value(), ')', "）");
    add(config_.symbolBracket.value(), '[', "【");
    add(config_.symbolBracket.value(), ']', "】");
    add(config_.symbolBrace.value(), '{', "「");
    add(config_.symbolBrace.value(), '}', "」");
    add(config_.symbolAngle.value(), '<', "《");
    add(config_.symbolAngle.value(), '>', "》");
    add(config_.symbolBackslash.value(), '\\', "、");
}

void Engine::finishComposition(fcitx::InputContext *ic, State *state,
                               bool preferRaw) {
    if (state->preedit_.empty()) {
        return;
    }
    // "First candidate" means the first one on the page the user is looking at:
    // the list cursor is reset to 0 on every redraw, so after paging that is
    // exactly the highlighted entry.
    if (!preferRaw && state->hasCandidates_ &&
        selectCandidate(ic, state, pageBase(ic))) {
        return;
    }
    // Either the user asked for the literal pinyin, or there was nothing to
    // select. The composition has to end here: leaving it on screen is what
    // used to strand the user with uncommittable text.
    const std::string &raw = state->raw_.empty() ? state->preedit_ : state->raw_;
    ic->commitString(raw);
    resetSession(ic, state);
}

void Engine::updatePunctuationLabel() {
    // Full-width colon on purpose: an ASCII ':' inside a menu label is treated
    // as a "label: shortcut" separator and everything after it is dropped.
    const std::string text = config_.chinesePunctuation.value()
                                 ? "标点映射：中文"
                                 : "标点映射：英文";
    // shortText is the panel button caption, longText the menu item caption;
    // with only shortText set the tray entry renders as a blank icon.
    punctuationAction_.setShortText(text);
    punctuationAction_.setLongText(text);
    // Also a real checkbox in the menu: this is the switch users flip most
    // often, and the config page alone means opening configtool for it.
    punctuationAction_.setChecked(config_.chinesePunctuation.value());
}

void Engine::activate(const fcitx::InputMethodEntry &,
                      fcitx::InputContextEvent &event) {
    auto *ic = event.inputContext();
    // A pending bare-modifier press must not survive an input method switch:
    // the release would toggle the mode after the user already moved on.
    ic->propertyFor(&factory_)->switchArmed_ = false;
    // The config may have been changed from the config page since last time.
    updatePunctuationLabel();
    // InputMethod group is cleared before every activate, so re-adding each time
    // is the intended pattern.
    // Deliberately not removed in deactivate(): opening the status area takes
    // focus away from the client, which calls deactivate, and removing the
    // action there makes the menu entry flash and vanish. fcitx5 clears the
    // group on focus loss by itself.
    ic->statusArea().addAction(fcitx::StatusGroup::InputMethod,
                               &punctuationAction_);
}

void Engine::selectByIndex(fcitx::InputContext *ic, uint32_t index) {
    if (!handle_) {
        return;
    }
    auto *state = ic->propertyFor(&factory_);
    if (state->session() != 0) {
        selectCandidate(ic, state, index);
    }
}

void Engine::resetSession(fcitx::InputContext *ic, State *state) {
    if (state->session() != 0 &&
        wetype_reset(handle_.get(), state->session()) != 0) {
        // The reset never reached the engine, so its composition is still
        // there. The session id cannot be trusted any more; the next key opens
        // a fresh one instead of appending to a ghost.
        state->dropSession();
    }
    clearUI(ic, state);
}

bool Engine::pageCandidates(fcitx::InputContext *ic, fcitx::KeySym sym) {
    auto list = ic->inputPanel().candidateList();
    if (!list) {
        return false;
    }
    auto *pageable = list->toPageable();
    if (!pageable) {
        return false;
    }
    const bool forward = sym == FcitxKey_Page_Down || sym == FcitxKey_Down;
    if (forward) {
        if (!pageable->hasNext()) {
            return false;
        }
        pageable->next();
    } else {
        if (!pageable->hasPrev()) {
            return false;
        }
        pageable->prev();
    }
    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
    return true;
}

void Engine::updateUI(fcitx::InputContext *ic, State *state,
                      const WetypeCandidates &candidates) {
    auto &panel = ic->inputPanel();
    state->preedit_ = asString(candidates.preedit, candidates.preedit_len);
    state->raw_ = asString(candidates.raw, candidates.raw_len);
    state->cursor_ = static_cast<int>(candidates.cursor);

    // Client-side display needs an application that renders a preedit itself.
    // Without the capability the text would simply vanish, so fall back to the
    // panel instead of trusting the preference.
    const bool clientSide =
        config_.preeditMode.value() == PreeditMode::Client &&
        ic->capabilityFlags().test(fcitx::CapabilityFlag::Preedit);

    // Only the still-composing part is ever shown. A partial selection commits
    // its text straight into the application, so repeating it here would both
    // duplicate it in client-preedit mode and keep the preedit alive after the
    // pinyin is gone - which is what made Backspace unable to reach the
    // committed text.
    // Text::size() counts *pieces*, not bytes - clamping to it pins the cursor
    // to the first byte. textLength() is the byte length that setCursor wants.
    fcitx::Text preedit(state->preedit_);
    preedit.setCursor(
        std::min<int>(state->cursor_, static_cast<int>(preedit.textLength())));

    if (clientSide) {
        panel.setClientPreedit(preedit);
        panel.setPreedit(fcitx::Text());
    } else {
        panel.setClientPreedit(fcitx::Text());
        panel.setPreedit(preedit);
    }

    if (candidates.items_len == 0) {
        panel.setCandidateList(nullptr);
    } else {
        auto list = std::make_unique<fcitx::CommonCandidateList>();
        list->setPageSize(static_cast<int>(kPageSize));
        // fcitx renders "1. " from the selection keys, spacing included, which
        // hand-written labels do not get.
        list->setSelectionKey(fcitx::Key::keyListFromString("1 2 3 4 5"));
        list->setCursorPositionAfterPaging(
            fcitx::CursorPositionAfterPaging::ResetToFirst);
        for (uintptr_t i = 0; i < candidates.items_len; ++i) {
            const auto &item = candidates.items[i];
            auto text = fcitx::Text(asString(item.text, item.text_len));
            if (item.comment_len > 0) {
                text.append(" ");
                text.append(asString(item.comment, item.comment_len),
                            fcitx::TextFormatFlag::DontCommit);
            }
            list->append(std::make_unique<SelectCandidateWord>(std::move(text), i, this));
        }
        list->setGlobalCursorIndex(0);
        panel.setCandidateList(std::move(list));
    }
    ic->updatePreedit();
    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
}

void Engine::clearUI(fcitx::InputContext *ic, State *state) {
    state->preedit_.clear();
    state->raw_.clear();
    state->cursor_ = 0;
    state->hasCandidates_ = false;
    auto &panel = ic->inputPanel();
    // `reset` clears the panel content; the client-side copy has to be cleared
    // explicitly or the application keeps drawing the composition.
    panel.setClientPreedit(fcitx::Text());
    panel.reset();
    ic->updatePreedit();
    ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
}

} // namespace wetype
