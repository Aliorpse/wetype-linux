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

/// Addon configuration, shown in fcitx5-configtool.
///
/// The language-switch key, the Enter/Space behaviour and every punctuation
/// mapping are user-visible preferences, so all of them live here rather than
/// being compiled in; changing any of them needs no rebuild.
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
    // The master switch for the per-symbol list below. It belongs directly
    // above that list: sitting between the switch key and Enter/Space made it
    // look like it governed those instead.
    fcitx::Option<bool> chinesePunctuation{
        this, "ChinesePunctuation", "中文标点（总开关）", true};
    // One checkbox per symbol, so the config page reads as a plain list.
    // Everything defaults to on; a key switched off stays ASCII.
    fcitx::Option<bool> symbolComma{this, "SymbolComma", "逗号  , → ，", true};
    fcitx::Option<bool> symbolPeriod{this, "SymbolPeriod", "句号  . → 。", true};
    fcitx::Option<bool> symbolSemicolon{this, "SymbolSemicolon", "分号  ; → ；", true};
    fcitx::Option<bool> symbolApostrophe{
        this, "SymbolApostrophe", "单引号  ' → ‘’（自动配对）", true};
    fcitx::Option<bool> symbolQuestion{this, "SymbolQuestion", "问号  ? → ？", true};
    fcitx::Option<bool> symbolExclam{this, "SymbolExclam", "叹号  ! → ！", true};
    fcitx::Option<bool> symbolColon{this, "SymbolColon", "冒号  : → ：", true};
    fcitx::Option<bool> symbolQuote{
        this, "SymbolQuote", "双引号  \" → “”（自动配对）", true};
    // One switch for both halves: nobody wants only the left one.
    fcitx::Option<bool> symbolParen{
        this, "SymbolParen", "括号  () → （）", true};
    fcitx::Option<bool> symbolBracket{
        this, "SymbolBracket", "方括号  [] → 【】", true};
    fcitx::Option<bool> symbolBrace{this, "SymbolBrace", "花括号  {} → 「」", true};
    fcitx::Option<bool> symbolAngle{this, "SymbolAngle", "书名号  <> → 《》", true};
    fcitx::Option<bool> symbolBackslash{
        this, "SymbolBackslash", "反斜杠  \\ → 、", true};
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
    bool ensureSession(fcitx::InputContext &ic);
    uint64_t session() const { return session_; }
    /// Connection generation the current session was created on. A different
    /// value means the daemon restarted and the session id is stale.
    uint64_t connection() const { return connection_; }
    void setConnection(uint64_t connection) { connection_ = connection; }
    /// Forget a session the daemon no longer knows about. Deliberately does not
    /// send CloseSession: the daemon that owned it is gone, and the id may
    /// already belong to someone else.
    void dropSession() { session_ = 0; }

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
    /// Toggle in fcitx5's status area / tray menu. Lives as long as the addon
    /// and must be registered with the UserInterfaceManager to be rendered.
    fcitx::SimpleAction punctuationAction_;
    WetypeConfig config_;
    std::map<char, std::string> punctuation_;
    /// Set after the "service missing" popup was shown, so a burst of keys does
    /// not repeat it; cleared when a session can be created again.
    bool serviceWarned_ = false;
};

} // namespace wetype
