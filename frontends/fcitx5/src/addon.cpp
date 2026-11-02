// Addon entry point. fcitx5 dlopens this module and calls the exported factory
// symbol whose name matches the addon name from `addon/wetype.conf`.

#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>

#include "engine.h"

namespace {

class Factory : public fcitx::AddonFactory {
public:
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override {
        return new wetype::Engine(manager);
    }
};

} // namespace

// V2 exports `fcitx_addon_factory_instance_wetype`; BACKWARDS also exports the
// legacy unsuffixed symbol so older loaders still find us.
FCITX_ADDON_FACTORY_V2_BACKWARDS(wetype, Factory)
