#pragma once
#include "base_visual_player.hpp"

class VisualPlayer : public cocos2d::CCNode, public BaseVisualPlayer {
public:
    bool init(RemotePlayer* parent, bool isSecond) override;
    void updateIcons(const PlayerIconData& icons) override;
    void updateData(const SpecificIconData& data) override;
    void updateName() override;
    void updateIconType(PlayerIconType newType) override;

    static VisualPlayer* create(RemotePlayer* parent, bool isSecond);

protected:
    PlayLayer* playLayer;
    SimplePlayer* playerIcon;
    PlayerIconType playerIconType;
    cocos2d::CCLabelBMFont* playerName;
};