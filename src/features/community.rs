//! Typed group, channel, invitation, and channel-post operations.
//!
//! The native core owns transport, authentication, and wire decoding. These
//! synchronous functions must be dispatched away from the Slint event loop.
//! Identifiers and role/permission validation remain enforced by the core and
//! server; this module preserves their Rust `snake_case` request contract.

use friendshub_core::{Client, error::Result as CoreResult};
use serde::{Deserialize, Serialize, de::IgnoredAny};

#[derive(Serialize)]
struct IdRequest<'a> {
    id: &'a str,
}

#[derive(Serialize)]
struct ConversationRequest<'a> {
    conversation_id: &'a str,
}

macro_rules! id_mutation {
    ($name:ident, $method:ident) => {
        pub fn $name(client: &Client, id: &str) -> CoreResult<()> {
            let _: IgnoredAny = client.$method(&IdRequest { id })?;
            Ok(())
        }
    };
}

macro_rules! conversation_mutation {
    ($name:ident, $method:ident) => {
        pub fn $name(client: &Client, conversation_id: &str) -> CoreResult<()> {
            let _: IgnoredAny = client.$method(&ConversationRequest { conversation_id })?;
            Ok(())
        }
    };
}

/// Administrator bits used by WEB's group rights editor. The owner holds all
/// rights independently of the stored mask.
pub mod group_permissions {
    pub const CHANGE_PROFILE: i64 = 1;
    pub const MANAGE_GREETINGS: i64 = 2;
    pub const DELETE_MESSAGES: i64 = 4;
    pub const BAN_USERS: i64 = 8;
    pub const MANAGE_INVITES: i64 = 16;
    pub const PIN_MESSAGES: i64 = 32;
    pub const MANAGE_VIDEO_CHATS: i64 = 64;
    pub const CHANGE_TAGS: i64 = 128;
    pub const ANONYMOUS_POST: i64 = 256;
    pub const ADD_ADMINS: i64 = 512;
    pub const ALL: i64 = 1023;
}

#[derive(Serialize)]
pub struct CreateGroup<'a> {
    pub title: &'a str,
    pub member_ids: &'a [String],
    pub is_public: bool,
    pub description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_seconds: Option<i64>,
}

#[derive(Clone, Deserialize)]
pub struct GroupCreated {
    pub conversation_id: String,
    pub title: Option<String>,
    pub is_public: bool,
    pub description: Option<String>,
    pub owner_account_id: String,
    pub created_at: i64,
}

pub fn create_group(client: &Client, input: &CreateGroup<'_>) -> CoreResult<GroupCreated> {
    client.groups_create(input)
}

#[derive(Clone, Deserialize)]
pub struct GroupDetail {
    pub conversation_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub is_public: bool,
    pub owner_account_id: String,
    pub handle: Option<String>,
    pub handle_normalized: Option<String>,
    pub member_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub avatar_url: Option<String>,
}

pub fn group(client: &Client, conversation_id: &str) -> CoreResult<GroupDetail> {
    client.community_extras_get_group(&ConversationRequest { conversation_id })
}

/// `None` leaves a field unchanged. An empty description explicitly clears
/// its visible text, matching the WEB group metadata editor.
#[derive(Serialize)]
pub struct GroupPatch<'a> {
    pub conversation_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_public: Option<bool>,
}

pub fn update_group(client: &Client, input: &GroupPatch<'_>) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_update_group(input)?;
    Ok(())
}

conversation_mutation!(delete_group, community_extras_delete_group);

#[derive(Serialize)]
struct TransferOwner<'a> {
    conversation_id: &'a str,
    new_owner_account_id: &'a str,
}

pub fn transfer_group_owner(
    client: &Client,
    conversation_id: &str,
    new_owner_account_id: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_transfer_group_owner(&TransferOwner {
        conversation_id,
        new_owner_account_id,
    })?;
    Ok(())
}

#[derive(Clone, Deserialize)]
pub struct GroupMember {
    pub account_id: String,
    pub role: String,
    pub permissions: i64,
    pub muted_until: Option<i64>,
    pub moderator_muted_until: Option<i64>,
    pub joined_at: i64,
    pub group_tag: Option<String>,
}

pub fn group_members(client: &Client, conversation_id: &str) -> CoreResult<Vec<GroupMember>> {
    client.groups_list_members(&IdRequest {
        id: conversation_id,
    })
}

#[derive(Serialize)]
struct GroupMemberRequest<'a> {
    conversation_id: &'a str,
    account_id: &'a str,
}

pub fn add_group_member(
    client: &Client,
    conversation_id: &str,
    account_id: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_add_group_member(&GroupMemberRequest {
        conversation_id,
        account_id,
    })?;
    Ok(())
}

pub fn remove_group_member(
    client: &Client,
    conversation_id: &str,
    account_id: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_remove_group_member(&GroupMemberRequest {
        conversation_id,
        account_id,
    })?;
    Ok(())
}

#[derive(Serialize)]
struct GroupRoleRequest<'a> {
    conversation_id: &'a str,
    target_account_id: &'a str,
    role: &'a str,
    permissions: i64,
}

pub fn set_group_member_role(
    client: &Client,
    conversation_id: &str,
    target_account_id: &str,
    role: &str,
    permissions: i64,
) -> CoreResult<()> {
    let _: IgnoredAny = client.groups_set_role(&GroupRoleRequest {
        conversation_id,
        target_account_id,
        role,
        permissions,
    })?;
    Ok(())
}

#[derive(Serialize)]
struct MuteGroupMember<'a> {
    conversation_id: &'a str,
    target_account_id: &'a str,
    muted_until: Option<i64>,
}

pub fn mute_group_member(
    client: &Client,
    conversation_id: &str,
    target_account_id: &str,
    muted_until: Option<i64>,
) -> CoreResult<()> {
    let _: IgnoredAny = client.groups_mute_member(&MuteGroupMember {
        conversation_id,
        target_account_id,
        muted_until,
    })?;
    Ok(())
}

#[derive(Serialize)]
struct GroupTagRequest<'a> {
    conversation_id: &'a str,
    account_id: &'a str,
    tag: &'a str,
}

pub fn set_group_member_tag(
    client: &Client,
    conversation_id: &str,
    account_id: &str,
    tag: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_set_group_member_tag(&GroupTagRequest {
        conversation_id,
        account_id,
        tag,
    })?;
    Ok(())
}

pub fn remove_group_member_tag(
    client: &Client,
    conversation_id: &str,
    account_id: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_remove_group_member_tag(&GroupMemberRequest {
        conversation_id,
        account_id,
    })?;
    Ok(())
}

#[derive(Clone, Deserialize)]
pub struct GroupBan {
    pub account_id: String,
    pub banned_by_account_id: Option<String>,
    pub reason: Option<String>,
    pub banned_at: i64,
}

pub fn group_bans(client: &Client, conversation_id: &str) -> CoreResult<Vec<GroupBan>> {
    client.community_extras_list_group_bans(&ConversationRequest { conversation_id })
}

#[derive(Serialize)]
struct BanGroupMember<'a> {
    conversation_id: &'a str,
    account_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

pub fn ban_group_member(
    client: &Client,
    conversation_id: &str,
    account_id: &str,
    reason: Option<&str>,
) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_ban_group_member(&BanGroupMember {
        conversation_id,
        account_id,
        reason,
    })?;
    Ok(())
}

pub fn unban_group_member(
    client: &Client,
    conversation_id: &str,
    account_id: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_unban_group_member(&GroupMemberRequest {
        conversation_id,
        account_id,
    })?;
    Ok(())
}

#[derive(Serialize)]
struct HandleRequest<'a> {
    conversation_id: &'a str,
    handle: &'a str,
}

#[derive(Clone, Deserialize)]
pub struct CommunityHandle {
    pub handle: String,
    pub handle_normalized: String,
}

pub fn set_group_handle(
    client: &Client,
    conversation_id: &str,
    handle: &str,
) -> CoreResult<CommunityHandle> {
    client.groups_set_user_profile(&HandleRequest {
        conversation_id,
        handle,
    })
}

#[derive(Serialize)]
pub struct CreateChannel<'a> {
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub is_public: bool,
}

#[derive(Clone, Deserialize)]
pub struct ChannelCreated {
    pub conversation_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub owner_account_id: String,
    pub linked_group_conversation_id: Option<String>,
    pub is_public: bool,
    pub created_at: i64,
}

pub fn create_channel(client: &Client, input: &CreateChannel<'_>) -> CoreResult<ChannelCreated> {
    client.channels_create(input)
}

#[derive(Clone, Deserialize)]
pub struct ChannelDetail {
    pub conversation_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub is_public: bool,
    pub owner_account_id: String,
    pub linked_group_conversation_id: Option<String>,
    pub handle: Option<String>,
    pub handle_normalized: Option<String>,
    pub member_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub avatar_url: Option<String>,
}

pub fn channel(client: &Client, conversation_id: &str) -> CoreResult<ChannelDetail> {
    client.community_extras_get_channel(&ConversationRequest { conversation_id })
}

conversation_mutation!(delete_channel, community_extras_delete_channel);

pub fn transfer_channel_owner(
    client: &Client,
    conversation_id: &str,
    new_owner_account_id: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.community_extras_transfer_channel_owner(&TransferOwner {
        conversation_id,
        new_owner_account_id,
    })?;
    Ok(())
}

#[derive(Clone, Deserialize)]
pub struct ChannelMember {
    pub actor_type: String,
    pub actor_id: String,
    pub role: String,
    pub joined_at: i64,
}

pub fn channel_members(client: &Client, conversation_id: &str) -> CoreResult<Vec<ChannelMember>> {
    client.channels_list_members(&IdRequest {
        id: conversation_id,
    })
}

#[derive(Serialize)]
struct ChannelRoleRequest<'a> {
    conversation_id: &'a str,
    actor_type: &'a str,
    actor_id: &'a str,
    role: &'a str,
}

pub fn set_channel_member_role(
    client: &Client,
    conversation_id: &str,
    actor_type: &str,
    actor_id: &str,
    role: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.channels_set_role(&ChannelRoleRequest {
        conversation_id,
        actor_type,
        actor_id,
        role,
    })?;
    Ok(())
}

#[derive(Serialize)]
struct ChannelMemberRequest<'a> {
    conversation_id: &'a str,
    actor_type: &'a str,
    actor_id: &'a str,
}

pub fn remove_channel_member(
    client: &Client,
    conversation_id: &str,
    actor_type: &str,
    actor_id: &str,
) -> CoreResult<()> {
    let _: IgnoredAny = client.channels_remove_member(&ChannelMemberRequest {
        conversation_id,
        actor_type,
        actor_id,
    })?;
    Ok(())
}

#[derive(Serialize)]
struct LinkedGroupRequest<'a> {
    conversation_id: &'a str,
    linked_group_conversation_id: Option<&'a str>,
}

/// Passing `None` detaches the channel's discussion group.
pub fn set_linked_group(
    client: &Client,
    conversation_id: &str,
    linked_group_conversation_id: Option<&str>,
) -> CoreResult<()> {
    let _: IgnoredAny = client.channels_set_linked_group(&LinkedGroupRequest {
        conversation_id,
        linked_group_conversation_id,
    })?;
    Ok(())
}

#[derive(Serialize)]
struct ChannelPublicRequest<'a> {
    conversation_id: &'a str,
    is_public: bool,
}

pub fn set_channel_public(
    client: &Client,
    conversation_id: &str,
    is_public: bool,
) -> CoreResult<()> {
    let _: IgnoredAny = client.channels_set_public(&ChannelPublicRequest {
        conversation_id,
        is_public,
    })?;
    Ok(())
}

pub fn set_channel_handle(
    client: &Client,
    conversation_id: &str,
    handle: &str,
) -> CoreResult<CommunityHandle> {
    client.channels_set_user_profile(&HandleRequest {
        conversation_id,
        handle,
    })
}

#[derive(Serialize)]
struct ChannelBotRequest<'a> {
    conversation_id: &'a str,
    bot_id: &'a str,
}

pub fn add_channel_bot(client: &Client, conversation_id: &str, bot_id: &str) -> CoreResult<()> {
    let _: IgnoredAny = client.channels_add_bot(&ChannelBotRequest {
        conversation_id,
        bot_id,
    })?;
    Ok(())
}

#[derive(Deserialize)]
struct PublishPermission {
    can_publish: bool,
}

pub fn can_publish(client: &Client, conversation_id: &str) -> CoreResult<bool> {
    let response: PublishPermission = client.channels_can_publish(&IdRequest {
        id: conversation_id,
    })?;
    Ok(response.can_publish)
}

id_mutation!(subscribe_channel, channels_subscribe);
id_mutation!(unsubscribe_channel, channels_unsubscribe);

#[derive(Clone, Deserialize)]
pub struct ChannelAnalytics {
    pub posts: Vec<PostViews>,
}

#[derive(Clone, Deserialize)]
pub struct PostViews {
    pub post_id: String,
    pub views: i64,
}

pub fn channel_analytics(client: &Client, conversation_id: &str) -> CoreResult<ChannelAnalytics> {
    client.community_extras_get_channel_analytics(&ConversationRequest { conversation_id })
}

#[derive(Clone, Deserialize)]
pub struct Invite {
    pub id: String,
    pub token: String,
    pub kind: String,
    pub used_count: i64,
    pub max_uses: Option<i64>,
    pub created_at: i64,
}

#[derive(Serialize)]
struct CreateInviteRequest<'a> {
    conversation_id: &'a str,
    kind: &'a str,
}

pub fn create_invite(client: &Client, conversation_id: &str, kind: &str) -> CoreResult<Invite> {
    client.invites_create(&CreateInviteRequest {
        conversation_id,
        kind,
    })
}

pub fn invites(client: &Client, conversation_id: &str) -> CoreResult<Vec<Invite>> {
    client.invites_list(&IdRequest {
        id: conversation_id,
    })
}

id_mutation!(revoke_invite, invites_revoke);

#[derive(Serialize)]
struct InviteTokenRequest<'a> {
    token: &'a str,
}

#[derive(Deserialize)]
pub struct InviteJoined {
    pub conversation_id: String,
}

pub fn join_invite(client: &Client, token: &str) -> CoreResult<InviteJoined> {
    client.invites_join_by_token(&InviteTokenRequest { token })
}

#[derive(Clone, Deserialize)]
pub struct ChannelPost {
    pub id: String,
    pub channel_id: String,
    pub author_type: String,
    pub author_id: String,
    pub text: String,
    pub attachment_ids: Vec<String>,
    pub forward_from: Option<String>,
    pub reply_to_post_id: Option<String>,
    pub created_at: i64,
    pub edited_at: Option<i64>,
    pub pinned_at: Option<i64>,
    pub discussion_comment_count: i64,
}

#[derive(Serialize)]
pub struct PublishPost<'a> {
    pub channel_id: &'a str,
    pub text: &'a str,
    pub attachment_ids: &'a [String],
    pub reply_to_post_id: Option<&'a str>,
    pub forward_from: Option<&'a str>,
}

pub fn publish_post(client: &Client, input: &PublishPost<'_>) -> CoreResult<ChannelPost> {
    client.channel_posts_publish(input)
}

#[derive(Serialize)]
struct PostsRequest<'a> {
    channel_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<i64>,
}

pub fn channel_posts(
    client: &Client,
    channel_id: &str,
    before: Option<&str>,
    limit: Option<i64>,
) -> CoreResult<Vec<ChannelPost>> {
    client.channel_posts_list(&PostsRequest {
        channel_id,
        before,
        limit,
    })
}

pub fn channel_post(client: &Client, post_id: &str) -> CoreResult<ChannelPost> {
    client.channel_posts_get_one(&IdRequest { id: post_id })
}

#[derive(Serialize)]
struct EditPostRequest<'a> {
    post_id: &'a str,
    text: &'a str,
}

pub fn edit_post(client: &Client, post_id: &str, text: &str) -> CoreResult<()> {
    let _: IgnoredAny = client.channel_posts_edit(&EditPostRequest { post_id, text })?;
    Ok(())
}

id_mutation!(delete_post, channel_posts_delete);
id_mutation!(pin_post, channel_posts_pin);
id_mutation!(unpin_post, channel_posts_unpin);

pub fn pinned_posts(client: &Client, channel_id: &str) -> CoreResult<Vec<ChannelPost>> {
    client.channel_posts_list_pinned(&IdRequest { id: channel_id })
}

#[derive(Serialize)]
struct PostReactionRequest<'a> {
    post_id: &'a str,
    emoji: &'a str,
}

pub fn set_post_reaction(client: &Client, post_id: &str, emoji: &str) -> CoreResult<()> {
    let _: IgnoredAny =
        client.channel_posts_set_reaction(&PostReactionRequest { post_id, emoji })?;
    Ok(())
}

id_mutation!(remove_post_reaction, channel_posts_remove_reaction);

#[derive(Clone, Deserialize)]
pub struct PostReaction {
    pub actor_type: String,
    pub actor_id: String,
    pub emoji: String,
    pub created_at: i64,
}

#[derive(Serialize)]
struct PostReactionsRequest<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    before_created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    before_actor_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<i64>,
}

/// When `limit` is omitted, core uses WEB's page size of 200 reactions.
pub fn post_reactions(
    client: &Client,
    post_id: &str,
    before_created_at: Option<i64>,
    before_actor_id: Option<&str>,
    limit: Option<i64>,
) -> CoreResult<Vec<PostReaction>> {
    client.channel_posts_list_reactions(&PostReactionsRequest {
        id: post_id,
        before_created_at,
        before_actor_id,
        limit,
    })
}

#[derive(Clone, Deserialize)]
pub struct PostDiscussion {
    pub channel_id: String,
    pub post_id: String,
    pub linked_group_id: Option<String>,
    pub thread_id: String,
    pub comment_count: i64,
}

pub fn post_discussion(client: &Client, post_id: &str) -> CoreResult<PostDiscussion> {
    client.channel_posts_get_discussion(&IdRequest { id: post_id })
}

#[derive(Serialize)]
struct CommunityAvatarRequest<'a> {
    conversation_id: &'a str,
    file_path: &'a str,
}

#[derive(Deserialize)]
pub struct AvatarUploaded {
    pub avatar_url: String,
}

/// `file_path` must be an absolute path to a WebP file readable by core.
/// Picker/content-URI resolution and image conversion belong to the host.
pub fn upload_group_avatar(
    client: &Client,
    conversation_id: &str,
    file_path: &str,
) -> CoreResult<AvatarUploaded> {
    client.avatars_upload_group(&CommunityAvatarRequest {
        conversation_id,
        file_path,
    })
}

pub fn upload_channel_avatar(
    client: &Client,
    conversation_id: &str,
    file_path: &str,
) -> CoreResult<AvatarUploaded> {
    client.avatars_upload_channel(&CommunityAvatarRequest {
        conversation_id,
        file_path,
    })
}

conversation_mutation!(delete_group_avatar, avatars_delete_group);
conversation_mutation!(delete_channel_avatar, avatars_delete_channel);
