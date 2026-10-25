//! Channel permission computation, following the algorithm in Discord's documentation
//! ("Permission Overwrites": base → @everyone overwrite → role overwrites → member overwrite).

use crate::model::{Channel, Guild, Id, Role};

pub const CREATE_INSTANT_INVITE: u64 = 1 << 0;
pub const ADMINISTRATOR: u64 = 1 << 3;
pub const ADD_REACTIONS: u64 = 1 << 6;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const MANAGE_MESSAGES: u64 = 1 << 13;
pub const ATTACH_FILES: u64 = 1 << 15;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;
pub const ALL: u64 = u64::MAX;

pub fn base_permissions(guild_id: Id, owner_id: Id, roles: &[Role], user_id: Id, member_roles: &[Id]) -> u64 {
    if owner_id == user_id && user_id.is_set() {
        return ALL;
    }
    // The @everyone role shares the guild's id.
    let mut perms = roles.iter().find(|r| r.id == guild_id).map(|r| r.permissions).unwrap_or(0);
    for r in roles.iter().filter(|r| member_roles.contains(&r.id)) {
        perms |= r.permissions;
    }
    if perms & ADMINISTRATOR != 0 {
        return ALL;
    }
    perms
}

pub fn channel_permissions(base: u64, guild_id: Id, channel: &Channel, user_id: Id, member_roles: &[Id]) -> u64 {
    if base & ADMINISTRATOR != 0 {
        return ALL;
    }
    let mut perms = base;
    let ow = &channel.permission_overwrites;
    if let Some(e) = ow.iter().find(|o| o.id == guild_id) {
        perms &= !e.deny;
        perms |= e.allow;
    }
    let (mut allow, mut deny) = (0u64, 0u64);
    for o in ow.iter().filter(|o| o.kind == 0 && member_roles.contains(&o.id)) {
        allow |= o.allow;
        deny |= o.deny;
    }
    perms &= !deny;
    perms |= allow;
    if let Some(m) = ow.iter().find(|o| o.kind == 1 && o.id == user_id) {
        perms &= !m.deny;
        perms |= m.allow;
    }
    perms
}

/// Computes `my_permissions` for every channel of `guild` for `user_id`.
/// Threads inherit their parent's permissions.
pub fn annotate(guild: &mut Guild, user_id: Id) {
    let my_roles: Vec<Id> = guild
        .members
        .iter()
        .find(|m| m.user.as_ref().is_some_and(|u| u.id == user_id))
        .map(|m| m.roles.clone())
        .unwrap_or_default();
    let base = base_permissions(guild.id, guild.owner_id, &guild.roles, user_id, &my_roles);
    for c in guild.channels.iter_mut() {
        c.my_permissions = channel_permissions(base, guild.id, c, user_id, &my_roles);
    }
    let parents: Vec<(Id, u64)> = guild.channels.iter().map(|c| (c.id, c.my_permissions)).collect();
    for t in guild.threads.iter_mut() {
        t.my_permissions = t
            .parent_id
            .and_then(|p| parents.iter().find(|(id, _)| *id == p).map(|(_, perms)| *perms))
            .unwrap_or(base);
    }
}

pub fn has(perms: u64, flag: u64) -> bool {
    perms & flag == flag
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Member, Overwrite, User};

    fn role(id: u64, perms: u64) -> Role {
        Role { id: Id(id), permissions: perms, ..Default::default() }
    }

    #[test]
    fn overwrite_order_matches_discord() {
        let g = Id(1);
        let me = Id(50);
        let roles = vec![role(1, VIEW_CHANNEL | SEND_MESSAGES), role(2, 0)];
        let base = base_permissions(g, Id(99), &roles, me, &[Id(2)]);
        assert!(has(base, VIEW_CHANNEL));
        // @everyone denies view, role 2 allows it back, member overwrite denies send.
        let ch = Channel {
            permission_overwrites: vec![
                Overwrite { id: g, kind: 0, allow: 0, deny: VIEW_CHANNEL | SEND_MESSAGES },
                Overwrite { id: Id(2), kind: 0, allow: VIEW_CHANNEL | SEND_MESSAGES, deny: 0 },
                Overwrite { id: me, kind: 1, allow: 0, deny: SEND_MESSAGES },
            ],
            ..Default::default()
        };
        let p = channel_permissions(base, g, &ch, me, &[Id(2)]);
        assert!(has(p, VIEW_CHANNEL));
        assert!(!has(p, SEND_MESSAGES));
    }

    #[test]
    fn admin_and_owner_bypass() {
        let roles = vec![role(1, 0), role(3, ADMINISTRATOR)];
        assert_eq!(base_permissions(Id(1), Id(9), &roles, Id(5), &[Id(3)]), ALL);
        assert_eq!(base_permissions(Id(1), Id(5), &roles, Id(5), &[]), ALL);
    }

    #[test]
    fn annotate_threads_inherit_parent() {
        let me = Id(50);
        let mut g = Guild {
            id: Id(1),
            roles: vec![role(1, VIEW_CHANNEL)],
            members: vec![Member { user: Some(User { id: me, ..Default::default() }), ..Default::default() }],
            channels: vec![Channel {
                id: Id(10),
                permission_overwrites: vec![Overwrite { id: Id(1), kind: 0, allow: 0, deny: VIEW_CHANNEL }],
                ..Default::default()
            }],
            threads: vec![Channel { id: Id(11), parent_id: Some(Id(10)), ..Default::default() }],
            ..Default::default()
        };
        annotate(&mut g, me);
        assert!(!has(g.channels[0].my_permissions, VIEW_CHANNEL));
        assert!(!has(g.threads[0].my_permissions, VIEW_CHANNEL));
    }
}
