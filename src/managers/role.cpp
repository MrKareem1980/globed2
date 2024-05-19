#include "role.hpp"

void RoleManager::setAllRoles(std::vector<GameServerRole>&& allRoles) {
    this->allRoles = std::move(allRoles);
}

void RoleManager::setAllRoles(const std::vector<GameServerRole>& allRoles) {
    this->allRoles = allRoles;
}

void RoleManager::clearAllRoles() {
    allRoles.clear();
}

std::vector<GameServerRole>& RoleManager::getAllRoles() {
    return allRoles;
}

ComputedRole RoleManager::compute(const std::vector<uint8_t>& roles) {
    ComputedRole computed = {};
    computed.priority = INT_MIN;

    for (auto& roleid : roles) {
        auto it = std::find_if(allRoles.begin(), allRoles.end(), [&](auto& role) { return role.intId == roleid; });
        if (it == allRoles.end()) continue;

        auto& role = it->role;

        bool isHigher = role.priority > computed.priority;

        if (!role.badgeIcon.empty() && (isHigher || computed.badgeIcon.empty())) {
            computed.badgeIcon = role.badgeIcon;
        }

        if (!role.nameColor.empty() && (isHigher || !computed.nameColor.has_value())) {
            auto col = RichColor::parse(role.nameColor);
            if (!col) {
                log::warn("failed to parse color: {}", col.unwrapErr());
            }

            computed.nameColor = col.ok();
        }

        if (!role.chatColor.empty() && (isHigher || !computed.chatColor.has_value())) {
            computed.chatColor = geode::cocos::cc3bFromHexString(role.chatColor).ok();
        }

        // NOTE: we intentionally dont compute permissions client-side as it's not required

        if (isHigher) {
            computed.priority = role.priority;
        }
    }

    return computed;
}