// C++ side of the fcitx5 addon.
//
// This frontend owns fcitx5-facing behavior: key routing, mode switching,
// punctuation and commit policy, plus presentation of engine candidates. The
// engine remains responsible for composition and candidate segmentation.

#pragma once

#include <cstdint>
#include <map>
#include <memory>
#include <string>

#include <fcitx/action.h>
#include <fcitx/addonmanager.h>
#include <fcitx/inputcontextproperty.h>
#include <fcitx/inputmethodengine.h>

#include <fcitx-config/configuration.h>
#include <fcitx-config/enum.h>
#include <fcitx-config/iniparser.h>

// Generated from the Rust C ABI; the build writes it into the build directory
// and the checked-in copy is the fallback for non-Meson consumers.
#include "wetype.h"

namespace wetype {

/// Which bare modifier toggles Chinese/English. Both hands are accepted.
FCITX_CONFIG_ENUM(LanguageSwitch, Shift, Ctrl, Both);

/// What Enter and Space do while a composition is open.
///
/// The value names double as the dropdown labels and as the strings written to
/// the config file: fcitx5 only translates enum labels through a gettext
/// catalogue, and this addon ships none, so Chinese names would otherwise be
/// impossible to show. That is why this uses ENUM_NAME rather than ENUM.
enum class CommitBehavior { Raw, FirstCandidate };
FCITX_CONFIG_ENUM_NAME(CommitBehavior, "提交原始输入", "选第一个候选");

/// Where the composition string is drawn.
///
/// Panel is what fcitx5 addons normally do: the text is drawn by fcitx5's own
/// input window, which works in every application. Client lets the application
/// render it inline (the way native IMEs look), but only clients that advertise
/// `CapabilityFlag::Preedit` can do that, so it falls back to the panel.
enum class PreeditMode { Panel, Client };
FCITX_CONFIG_ENUM_NAME(PreeditMode, "fcitx5 显示", "应用内显示");

/// Per-symbol punctuation switches.
///
/// A nested Configuration is rendered by fcitx5-configtool as one titled group
/// box, so the dozen switches do not flatten the addon page.
FCITX_CONFIGURATION(PunctuationConfig,
    fcitx::Option<bool> enabled{this, "Enabled", "中文标点（总开关）", true};
    fcitx::Option<bool> comma{this, "Comma", "逗号  , → ，", true};
    fcitx::Option<bool> period{this, "Period", "句号  . → 。", true};
    fcitx::Option<bool> semicolon{this, "Semicolon", "分号  ; → ；", true};
    fcitx::Option<bool> apostrophe{
        this, "Apostrophe", "单引号  ' → ‘’（自动配对）", true};
    fcitx::Option<bool> question{this, "Question", "问号  ? → ？", true};
    fcitx::Option<bool> exclam{this, "Exclam", "叹号  ! → ！", true};
    fcitx::Option<bool> colon{this, "Colon", "冒号  : → ：", true};
    fcitx::Option<bool> quote{this, "Quote", "双引号  \" → “”（自动配对）", true};
    // One switch for both halves: nobody wants only the left one.
    fcitx::Option<bool> paren{this, "Paren", "括号  () → （）", true};
    fcitx::Option<bool> bracket{this, "Bracket", "方括号  [] → 【】", true};
    fcitx::Option<bool> brace{this, "Brace", "花括号  {} → 「」", true};
    fcitx::Option<bool> angle{this, "Angle", "书名号  <> → 《》", true};
    fcitx::Option<bool> backslash{this, "Backslash", "反斜杠  \\ → 、", true};
);


/// InputAugmenter corrections, one group box.
///
/// The five bytes are independent in the engine; `enabled` is the master switch
/// exposed in the status area, and the sub-switches let a user keep, say, only
/// transposition while turning the rest off.
FCITX_CONFIGURATION(SmartInputConfig,
    fcitx::Option<bool> enabled{this, "Enabled", "智能纠错（总开关）", true};
    fcitx::Option<bool> transpose{this, "Transpose", "换位纠错", true};
    fcitx::Option<bool> neighbor{this, "Neighbor", "邻键纠错", true};
    fcitx::Option<bool> insert{this, "Insert", "插入纠错", true};
    fcitx::Option<bool> skip{this, "Skip", "跳字纠错", true};
    fcitx::Option<bool> other{this, "Other", "其他纠错", true};
);

/// Fuzzy-pinyin groups, one group box.
///
/// Only en/eng and in/ing are on by default. The whole an/ang family
/// (an/ang, ian/iang, uan/uang) is off: that distinction is clear enough that
/// fuzzy matching changes too many common words. Every other pair starts off.
FCITX_CONFIGURATION(FuzzyConfig,
    fcitx::Option<bool> nl{this, "nl", "n / l（la → 那）", false};
    fcitx::Option<bool> rl{this, "rl", "r / l（rou → 楼）", false};
    fcitx::Option<bool> hf{this, "hf", "h / f（hu → 付）", false};
    fcitx::Option<bool> gk{this, "gk", "g / k（ge → 可）", false};
    fcitx::Option<bool> an_ang{this, "an_ang", "an / ang（fan → 放）", false};
    fcitx::Option<bool> ian_iang{
        this, "ian_iang", "ian / iang（jian → 将）", false};
    fcitx::Option<bool> uan_uang{
        this, "uan_uang", "uan / uang（guan → 光）", false};
    fcitx::Option<bool> c_ch{this, "c_ch", "c / ch（cang → 长）", false};
    fcitx::Option<bool> s_sh{this, "s_sh", "s / sh（sang → 上）", false};
    fcitx::Option<bool> z_zh{this, "z_zh", "z / zh（zong → 中）", false};
    fcitx::Option<bool> hui_fei{this, "hui_fei", "hui / fei（hui → 费）", false};
    fcitx::Option<bool> en_eng{this, "en_eng", "en / eng（fen → 风）", true};
    fcitx::Option<bool> in_ing{this, "in_ing", "in / ing（jin → 经）", true};
    fcitx::Option<bool> on_ong{this, "on_ong", "on / ong", false};
    fcitx::Option<bool> huang_wang{
        this, "huang_wang", "huang / wang（huang → 王）", false};
    fcitx::Option<bool> un_ong{this, "un_ong", "un / ong（gun → 工）", false};
    fcitx::Option<bool> un_iong{this, "un_iong", "un / iong（jiong → 军）", false};
    fcitx::Option<bool> an_ai{this, "an_ai", "an / ai（an → 爱）", false};
    fcitx::Option<bool> eng_ong{this, "eng_ong", "eng / ong（deng → 东）", false};
);

/// Addon configuration, shown in fcitx5-configtool.
///
/// The language-switch key, the Enter/Space behaviour, every punctuation
/// mapping, and the smart-input/fuzzy-pinyin switches are user-visible
/// preferences, so all of them live here rather than being compiled in;
/// changing any of them needs no rebuild.
FCITX_CONFIGURATION(WetypeConfig,
    // One dropdown instead of a key capture widget: the widget insists on a key
    // plus modifiers, so a bare Shift/Ctrl can never be captured there. An enum
    // renders as a combobox and needs no key input at all.
    fcitx::Option<LanguageSwitch> languageSwitch{
        this, "LanguageSwitch", "中英文切换键", LanguageSwitch::Shift};
    fcitx::Option<PreeditMode> preeditMode{
        this, "PreeditMode", "预编辑显示方式", PreeditMode::Panel};
    // Enter and Space both finish a composition; which text they take is a
    // matter of taste, so both are dropdowns. The defaults are the usual
    // Chinese-IME behaviour: Enter keeps the literal pinyin, Space takes the
    // first candidate. Neither ever types a space while composing - an idle
    // Space is not touched at all and reaches the application.
    fcitx::Option<CommitBehavior> enterBehavior{
        this, "EnterBehavior", "回车键行为", CommitBehavior::Raw};
    fcitx::Option<CommitBehavior> spaceBehavior{
        this, "SpaceBehavior", "空格键行为", CommitBehavior::FirstCandidate};
    // The master switch lives inside the same group box as the per-symbol
    // switches it governs.
    fcitx::Option<PunctuationConfig> punctuation{
        this, "Punctuation", "符号映射", PunctuationConfig{}};
    fcitx::Option<SmartInputConfig> smartInput{
        this, "SmartInput", "智能纠错", SmartInputConfig{}};
    fcitx::Option<FuzzyConfig> fuzzy{
        this, "Fuzzy", "模糊音", FuzzyConfig{}};
);

/// Owns the daemon connection and closes it exactly once.
struct HandleDeleter {
    void operator()(WetypeHandle *handle) const {
        if (handle) {
            wetype_close(handle);
        }
    }
};
using HandlePtr = std::shared_ptr<WetypeHandle>;

/// Per-input-context state. One engine session per IC, closed when the IC dies.
class State : public fcitx::InputContextProperty {
public:
    explicit State(HandlePtr handle) : handle_(std::move(handle)) {}
    ~State() override;

    /// Lazily created on the first key, so switching input methods does not
    /// create engine sessions that are never used.
    bool ensureSession(fcitx::InputContext &ic, const std::string &smart,
                       const std::string &fuzzy, uint64_t configGeneration);
    uint64_t session() const { return session_; }
    /// Config revision the current session was created with. A different value
    /// means the smart/fuzzy options changed and the session must be rebuilt.
    uint64_t configGeneration() const { return configGeneration_; }
    /// Connection generation the current session was created on. A different
    /// value means the daemon restarted and the session id is stale.
    uint64_t connection() const { return connection_; }
    void setConnection(uint64_t connection) { connection_ = connection; }
    /// Forget the session and ask the daemon to close it.
    ///
    /// If the daemon restarted, the id is unknown to it; if the id belongs to
    /// another connection, the daemon rejects the close. Both cases are
    /// harmless, and a live session is not leaked.
    void dropSession() {
        if (session_ != 0 && handle_) {
            wetype_close_session(handle_.get(), session_);
        }
        session_ = 0;
    }

    /// Text of the first candidate in the current snapshot, used when a
    /// punctuation key has to finish the composition.
    std::string firstCandidate_;

    /// English mode: letters are not intercepted at all.
    bool english_ = false;
    /// A switch-key press is pending; any other key cancels it. Reset on
    /// release, which is when the switch actually happens.
    bool switchArmed_ = false;

    uint64_t generation_ = 0;
    std::string preedit_;
    /// Composition without display separators; committed on Enter.
    std::string raw_;
    int cursor_ = 0;
    bool hasCandidates_ = false;
    /// Quote keys alternate between the opening and closing form ("..." needs
    /// a closing quote). The parity belongs to the input context so two windows
    /// cannot desynchronise each other.
    bool openDoubleQuote_ = true;
    bool openSingleQuote_ = true;

private:
    HandlePtr handle_;
    uint64_t session_ = 0;
    uint64_t connection_ = 0;
    uint64_t configGeneration_ = 0;
};

class StateFactory : public fcitx::InputContextPropertyFactory {
public:
    using PropertyType = State;
    explicit StateFactory(HandlePtr handle) : handle_(std::move(handle)) {}
    fcitx::InputContextProperty *create(fcitx::InputContext &ic) override;

private:
    HandlePtr handle_;
};

class Engine : public fcitx::InputMethodEngineV2 {
public:
    explicit Engine(fcitx::AddonManager *manager);
    ~Engine() override;

    void keyEvent(const fcitx::InputMethodEntry &entry,
                  fcitx::KeyEvent &event) override;
    void reset(const fcitx::InputMethodEntry &entry,
               fcitx::InputContextEvent &event) override;
    void activate(const fcitx::InputMethodEntry &entry,
                  fcitx::InputContextEvent &event) override;

    const fcitx::Configuration *getConfig() const override { return &config_; }
    void setConfig(const fcitx::RawConfig &raw) override;
    void reloadConfig() override;
    void save() override;

    /// Selection entry point, also used by mouse clicks on the candidate list.
    void selectByIndex(fcitx::InputContext *ic, uint32_t index);

private:
    /// Push the engine's composition and candidate list into fcitx5's UI.
    void updateUI(fcitx::InputContext *ic, State *state,
                  const WetypeCandidates &candidates);
    void clearUI(fcitx::InputContext *ic, State *state);

    /// True when this press/release belongs to a configured switch key.
    bool isSwitchKey(const fcitx::Key &key) const;
    /// Keep the status-area labels in sync with the current mode.
    void updatePunctuationLabel();

    /// True while the daemon socket is connected. Cheap enough for a key path.
    bool daemonAvailable() const;
    /// Mode label for the switch popup, with the service state appended so a
    /// missing daemon is visible instead of silently doing nothing.
    std::string modeLabel(bool english) const;
    /// One-shot hint when typing fails because the daemon is missing. Cleared
    /// once a session can be created again.
    void warnServiceMissing(fcitx::InputContext *ic);

    /// Build the punctuation lookup from the per-symbol checkboxes.
    void rebuildPunctuation();
    /// Smart/fuzzy names for the next session, in the order the engine host
    /// maps them to SessionConfig bytes. Empty strings mean all off.
    std::string smartOptions() const;
    std::string fuzzyOptions() const;
    /// Keep the status-area smart-input action in sync with the config.
    void updateSmartAction();

    /// Close the composition on Enter/Space. `preferRaw` commits the literal
    /// pinyin, otherwise the first candidate wins when there is one.
    void finishComposition(fcitx::InputContext *ic, State *state, bool preferRaw);

    /// Copy the engine snapshot into the per-IC state (generation, candidates,
    /// first candidate). Shared by key replies and partial selections.
    void adoptCandidates(State *state, const WetypeCandidates &candidates);

    bool sendKey(fcitx::InputContext *ic, State *state, uint8_t key);
    /// Page the candidate list. Returns false when there is no page to move to,
    /// so the key can fall through to the application.
    bool pageCandidates(fcitx::InputContext *ic, fcitx::KeySym sym);
    bool sendBackspace(fcitx::InputContext *ic, State *state);
    /// First global index of the page currently shown; 0 when it cannot page.
    uint32_t pageBase(fcitx::InputContext *ic);
    /// `global` indexes the whole candidate list, not the visible page.
    bool selectCandidate(fcitx::InputContext *ic, State *state, uint32_t global);
    void resetSession(fcitx::InputContext *ic, State *state);

    HandlePtr handle_;
    StateFactory factory_;
    fcitx::Instance *instance_ = nullptr;
    /// Status-area toggles: the punctuation master switch and the smart-input
    /// master switch (fuzzy pinyin deliberately has no quick toggle). They live
    /// as long as the addon and must be registered with the
    /// UserInterfaceManager to be rendered.
    fcitx::SimpleAction punctuationAction_;
    fcitx::SimpleAction smartInputAction_;
    WetypeConfig config_;
    /// Bumped on every config reload; sessions created under an older revision
    /// are closed and recreated so smart/fuzzy changes take effect without
    /// restarting fcitx5.
    uint64_t configGeneration_ = 0;
    std::map<char, std::string> punctuation_;
    /// Set after the "service missing" popup was shown, so a burst of keys does
    /// not repeat it; cleared when a session can be created again.
    bool serviceWarned_ = false;
};

} // namespace wetype
