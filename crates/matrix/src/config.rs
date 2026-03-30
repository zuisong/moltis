use {
    moltis_channels::{
        config_view::ChannelConfigView,
        gating::{DmPolicy, GroupPolicy},
    },
    moltis_common::secret_serde,
    secrecy::Secret,
    serde::{Deserialize, Serialize, ser::SerializeStruct},
};

/// Configuration for a single Matrix account.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MatrixAccountConfig {
    /// Homeserver URL (e.g. "https://matrix.ponderosa.co").
    pub homeserver: String,

    /// Access token for authentication.
    #[serde(serialize_with = "secret_serde::serialize_secret")]
    pub access_token: Secret<String>,

    /// Matrix user ID (auto-detected from whoami if not set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    /// Device ID for session persistence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,

    /// DM access policy.
    pub dm_policy: DmPolicy,

    /// Room (group) access policy.
    pub room_policy: GroupPolicy,

    /// Room allowlist (room IDs or aliases).
    pub room_allowlist: Vec<String>,

    /// User allowlist (Matrix user IDs).
    pub user_allowlist: Vec<String>,

    /// Auto-join rooms on invite.
    pub auto_join: bool,

    /// Default model ID for this account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Provider name associated with `model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,

    /// Send responses as replies to the original message.
    pub reply_to_message: bool,

    /// Emoji reaction added while processing. None = disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ack_reaction: Option<String>,

    /// OTP self-approval for non-allowlisted DM users.
    pub otp_self_approval: bool,

    /// Cooldown in seconds after 3 failed OTP attempts.
    pub otp_cooldown_secs: u64,
}

impl Default for MatrixAccountConfig {
    fn default() -> Self {
        Self {
            homeserver: String::new(),
            access_token: Secret::new(String::new()),
            user_id: None,
            device_id: None,
            dm_policy: DmPolicy::Allowlist,
            room_policy: GroupPolicy::Allowlist,
            room_allowlist: Vec::new(),
            user_allowlist: Vec::new(),
            auto_join: true,
            model: None,
            model_provider: None,
            reply_to_message: true,
            ack_reaction: Some("\u{1f440}".into()), // 👀
            otp_self_approval: true,
            otp_cooldown_secs: 300,
        }
    }
}

impl std::fmt::Debug for MatrixAccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixAccountConfig")
            .field("homeserver", &self.homeserver)
            .field("access_token", &"[REDACTED]")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("dm_policy", &self.dm_policy)
            .field("room_policy", &self.room_policy)
            .field("room_allowlist", &self.room_allowlist)
            .field("user_allowlist", &self.user_allowlist)
            .field("auto_join", &self.auto_join)
            .field("model", &self.model)
            .field("model_provider", &self.model_provider)
            .field("reply_to_message", &self.reply_to_message)
            .field("ack_reaction", &self.ack_reaction)
            .field("otp_self_approval", &self.otp_self_approval)
            .field("otp_cooldown_secs", &self.otp_cooldown_secs)
            .finish()
    }
}

/// Wrapper that serializes secret fields as `"[REDACTED]"` for API responses.
pub struct RedactedConfig<'a>(pub &'a MatrixAccountConfig);

impl Serialize for RedactedConfig<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let c = self.0;
        let mut count = 10;
        count += c.user_id.is_some() as usize;
        count += c.device_id.is_some() as usize;
        count += c.model.is_some() as usize;
        count += c.model_provider.is_some() as usize;
        count += c.ack_reaction.is_some() as usize;

        let mut s = serializer.serialize_struct("MatrixAccountConfig", count)?;
        s.serialize_field("homeserver", &c.homeserver)?;
        s.serialize_field("access_token", secret_serde::REDACTED)?;
        if c.user_id.is_some() {
            s.serialize_field("user_id", &c.user_id)?;
        }
        if c.device_id.is_some() {
            s.serialize_field("device_id", &c.device_id)?;
        }
        s.serialize_field("dm_policy", &c.dm_policy)?;
        s.serialize_field("room_policy", &c.room_policy)?;
        s.serialize_field("room_allowlist", &c.room_allowlist)?;
        s.serialize_field("user_allowlist", &c.user_allowlist)?;
        s.serialize_field("auto_join", &c.auto_join)?;
        if c.model.is_some() {
            s.serialize_field("model", &c.model)?;
        }
        if c.model_provider.is_some() {
            s.serialize_field("model_provider", &c.model_provider)?;
        }
        s.serialize_field("reply_to_message", &c.reply_to_message)?;
        if c.ack_reaction.is_some() {
            s.serialize_field("ack_reaction", &c.ack_reaction)?;
        }
        s.serialize_field("otp_self_approval", &c.otp_self_approval)?;
        s.serialize_field("otp_cooldown_secs", &c.otp_cooldown_secs)?;
        s.end()
    }
}

impl ChannelConfigView for MatrixAccountConfig {
    fn allowlist(&self) -> &[String] {
        &self.user_allowlist
    }

    fn group_allowlist(&self) -> &[String] {
        &self.room_allowlist
    }

    fn dm_policy(&self) -> DmPolicy {
        self.dm_policy.clone()
    }

    fn group_policy(&self) -> GroupPolicy {
        self.room_policy.clone()
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn model_provider(&self) -> Option<&str> {
        self.model_provider.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip() {
        let json = serde_json::json!({
            "homeserver": "https://matrix.example.com",
            "access_token": "syt_test_token",
            "dm_policy": "allowlist",
            "room_policy": "allowlist",
            "room_allowlist": ["!room:example.com"],
            "user_allowlist": ["@alice:example.com"],
            "auto_join": true,
            "reply_to_message": true,
            "ack_reaction": "\u{1f440}",
            "otp_self_approval": true,
            "otp_cooldown_secs": 300,
        });
        let cfg: MatrixAccountConfig = serde_json::from_value(json).expect("parse failed");
        assert_eq!(cfg.homeserver, "https://matrix.example.com");
        assert_eq!(cfg.dm_policy, DmPolicy::Allowlist);
        assert_eq!(cfg.room_allowlist, vec!["!room:example.com"]);

        let value = serde_json::to_value(&cfg).expect("serialize failed");
        assert_eq!(value["access_token"], "syt_test_token");
        let _: MatrixAccountConfig = serde_json::from_value(value).expect("re-parse failed");
    }

    #[test]
    fn config_defaults() {
        let cfg = MatrixAccountConfig::default();
        assert_eq!(cfg.dm_policy, DmPolicy::Allowlist);
        assert_eq!(cfg.room_policy, GroupPolicy::Allowlist);
        assert!(cfg.auto_join);
        assert!(cfg.reply_to_message);
        assert_eq!(cfg.ack_reaction.as_deref(), Some("\u{1f440}"));
        assert!(cfg.otp_self_approval);
        assert_eq!(cfg.otp_cooldown_secs, 300);
    }

    #[test]
    fn debug_redacts_token() {
        let cfg = MatrixAccountConfig {
            access_token: Secret::new("super-secret".into()),
            ..Default::default()
        };
        let debug = format!("{cfg:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("super-secret"));
    }

    #[test]
    fn redacted_config_hides_access_token() {
        let cfg = MatrixAccountConfig {
            access_token: Secret::new("super-secret".into()),
            ..Default::default()
        };

        let value = serde_json::to_value(RedactedConfig(&cfg)).expect("serialize failed");

        assert_eq!(value["access_token"], "[REDACTED]");
        assert!(!value.to_string().contains("super-secret"));
    }
}
