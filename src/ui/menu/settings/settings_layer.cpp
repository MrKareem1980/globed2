#include "settings_layer.hpp"

#include <util/ui.hpp>
#include <managers/settings.hpp>

using namespace geode::prelude;

bool GlobedSettingsLayer::init() {
    if (!CCLayer::init()) return false;

    auto winsize = CCDirector::get()->getWinSize();

    auto listview = Build<ListView>::create(CCArray::create(), GlobedSettingCell::CELL_HEIGHT, LIST_WIDTH, LIST_HEIGHT)
        .collect();

    Build<GJListLayer>::create(listview, "Settings", util::ui::BG_COLOR_TRANSPARENT, LIST_WIDTH, 220.f, 0)
        .zOrder(2)
        .anchorPoint(0.f, 0.f)
        .parent(this)
        .id("setting-list"_spr)
        .store(listLayer);

    Build<CCSprite>::createSpriteName("GJ_deleteBtn_001.png")
        .intoMenuItem([this](auto) {
            geode::createQuickPopup("Reset all settings", "Are you sure you want to reset all settings? This action is <cr>irreversible.</c>", "Cancel", "Ok", [this](auto, bool accepted) {
                if (accepted) {
                    GlobedSettings::get().resetToDefaults();
                    this->remakeList();
                }
            });
        })
        .pos(winsize.width - 30.f, 30.f)
        .intoNewParent(CCMenu::create())
        .pos(0.f, 0.f)
        .parent(this);

    listLayer->setPosition({winsize / 2 - listLayer->getScaledContentSize() / 2});

    util::ui::prepareLayer(this);

    this->remakeList();

    return true;
}

void GlobedSettingsLayer::keyBackClicked() {
    util::ui::navigateBack();
}

void GlobedSettingsLayer::remakeList() {
    if (listLayer->m_listView) listLayer->m_listView->removeFromParent();

    listLayer->m_listView = Build<ListView>::create(this->createSettingsCells(), GlobedSettingCell::CELL_HEIGHT, LIST_WIDTH, LIST_HEIGHT)
        .parent(listLayer)
        .collect();
}

#define MAKE_SETTING(cat, name, setname, setdesc) cells->addObject(GlobedSettingCell::create(&settings.cat.name, getCellType<decltype(settings.cat.name)>(), setname, setdesc, {}))
#define MAKE_SETTING_TYPE(cat, name, type, setname, setdesc) cells->addObject(GlobedSettingCell::create(&settings.cat.name, type, setname, setdesc, {}))
#define MAKE_SETTING_LIM(cat, name, setname, setdesc, ...) cells->addObject(GlobedSettingCell::create(&settings.cat.name, getCellType<decltype(settings.cat.name)>(), setname, setdesc, __VA_ARGS__))
#define MAKE_HEADER(name) cells->addObject(GlobedSettingHeaderCell::create(name))

CCArray* GlobedSettingsLayer::createSettingsCells() {
    using Type = GlobedSettingCell::Type;

    auto cells = CCArray::create();

    auto& settings = GlobedSettings::get();

    MAKE_HEADER("Globed");
    MAKE_SETTING(globed, autoconnect, "Autoconnect", "Automatically connect to the last connected server on launch.");
    MAKE_SETTING_LIM(globed, tpsCap, "TPS cap", "Maximum amount of packets per second sent between the client and the server. Useful only for very silly things.", {
        .intMin = 1,
        .intMax = 240,
    });
    MAKE_SETTING(globed, preloadAssets, "Preload assets", "Makes the loading screen take more time but prevents some of the random lagspikes when in a level.");
    MAKE_SETTING_TYPE(globed, fragmentationLimit, Type::PacketFragmentation, "Packet limit", "Press the \"Test\" button to calibrate the maximum packet size. Should fix some of the issues with players not appearing in a level.");

    MAKE_HEADER("Overlay");
    MAKE_SETTING(overlay, enabled, "Ping overlay", "Show a small overlay when in a level, displaying the current latency to the server.");
    MAKE_SETTING_LIM(overlay, opacity, "Overlay opacity", "Opacity of the displayed overlay.", {
        .floatMin = 0.f,
        .floatMax = 1.f
    });
    MAKE_SETTING(overlay, hideConditionally, "Hide conditionally", "Hide the ping overlay when not connected to a server or in a non-uploaded level, instead of showing a substitute message.");
    MAKE_SETTING_TYPE(overlay, position, Type::Corner, "Position", "Position of the overlay on the screen.");

#if GLOBED_VOICE_SUPPORT
    MAKE_HEADER("Communication");
    MAKE_SETTING(communication, voiceEnabled, "Voice chat", "Enables in-game voice chat. To talk, hold V when in a level. (keybind can be changed in game settings)");
    MAKE_SETTING(communication, voiceProximity, "Voice proximity", "In platformer mode, the loudness of other players will be determined by how close they are to you.");
    MAKE_SETTING(communication, classicProximity, "Classic proximity", "Same as voice proximity, but for classic levels (non-platformer).");
    MAKE_SETTING_LIM(communication, voiceVolume, "Voice volume", "Controls how loud other players are.", {
        .floatMin = 0.f,
        .floatMax = 2.f,
    });
    MAKE_SETTING(communication, onlyFriends, "Only friends", "When enabled, you won't hear players that are not on your friend list in-game.");
    MAKE_SETTING(communication, lowerAudioLatency, "Lower audio latency", "Decreases the audio buffer size by 2 times, reducing the latency but potentially causing audio issues.");
    MAKE_SETTING(communication, deafenNotification, "Deafen notification", "Shows a notification when you deafen & undeafen");
    MAKE_SETTING_TYPE(communication, audioDevice, Type::AudioDevice, "Audio device", "The input device used for recording your voice.");
    // MAKE_SETTING(communication, voiceLoopback, "Voice loopback", "When enabled, you will hear your own voice as you speak.");
#endif // GLOBED_VOICE_SUPPORT

    MAKE_HEADER("Level UI");
    MAKE_SETTING(levelUi, progressIndicators, "Progress icons", "Show small icons under the progressbar (or at the edge of the screen in platformer), indicating how far other players are in the level.");
    MAKE_SETTING_LIM(levelUi, progressOpacity, "Indicator opacity", "Changes the opacity of the icons that represent other players.", {
        .floatMin = 0.f,
        .floatMax = 1.f
    });
    MAKE_SETTING(levelUi, voiceOverlay, "Voice overlay", "Show a small overlay in the bottom right indicating currently speaking players.");

    MAKE_HEADER("Players");
    MAKE_SETTING_LIM(players, playerOpacity, "Opacity", "Opacity of other players.", {
        .floatMin = 0.f,
        .floatMax = 1.f,
    });
    MAKE_SETTING(players, showNames, "Player names", "Show names above players' icons.");
    MAKE_SETTING(players, dualName, "Dual name", "Show the name of the player on their secondary icon as well.");
    MAKE_SETTING_LIM(players, nameOpacity, "Name opacity", "Opacity of player names.", {
        .floatMin = 0.f,
        .floatMax = 1.f
    });
    MAKE_SETTING(players, statusIcons, "Status icons", "Show an icon above a player if they are paused, in practice mode, or currently speaking.");
    MAKE_SETTING(players, deathEffects, "Death effects", "Play a death effect whenever a player dies.");
    MAKE_SETTING(players, defaultDeathEffect, "Default death effect", "Replaces the death effects of all players with a default explosion effect.");
    MAKE_SETTING(players, hideNearby, "Hide nearby players", "Increases the transparency of players as they get closer to you, so that they don't obstruct your view.");

    return cells;
}

GlobedSettingsLayer* GlobedSettingsLayer::create() {
    auto ret = new GlobedSettingsLayer;
    if (ret->init()) {
        ret->autorelease();
        return ret;
    }

    delete ret;
    return nullptr;
}
