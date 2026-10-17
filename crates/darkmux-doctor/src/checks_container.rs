//! (#3275) Which user a dispatch container runs as: the operator's
//! `runtime.container_user` override, else the platform's answer. A value
//! that doesn't parse refuses every dispatch, so doctor fails it here first,
//! naming the setting and the fix.

use crate::{Check, Status};
use darkmux_platform::{ContainerUser, Platform};
use darkmux_types::config_access::Source;

pub fn check_container_user() -> Check {
    let (value, source) = darkmux_types::config_access::container_user_override_with_source();
    describe(value.as_deref().map(|v| (v, source)), darkmux_platform::current())
}

/// The row for an override and the tier it came from (`None` when unset) on
/// `platform`.
fn describe(override_value: Option<(&str, Source)>, platform: &dyn Platform) -> Check {
    let check = |status, message: String, hint: Option<String>| Check { name: NAME.into(), status, message, hint };
    let Some((value, source)) = override_value else {
        return check(
            Status::Pass,
            format!("{} (the `{}` platform's answer; `runtime.container_user` unset)", who(platform.container_user()), platform.id()),
            None,
        );
    };
    // The setting as the operator set it: the env var wins over the key.
    let setting = if source == Source::Env { "DARKMUX_CONTAINER_USER" } else { "runtime.container_user" };
    match ContainerUser::parse(value) {
        Ok(user) => check(
            Status::Pass,
            format!("{} (`{setting}` = `{value}`, over the `{}` platform's answer)", who(user), platform.id()),
            None,
        ),
        Err(why) => check(
            Status::Fail,
            format!("{setting} is `{value}`, which every dispatch refuses: {why}"),
            Some(
                "Set a numeric user, `darkmux config set runtime.container_user <uid>:<gid>` (`id -u` and `id -g`), \
                 or `image` for the runtime image's own user; or remove the key to let the platform decide."
                    .into(),
            ),
        ),
    }
}

/// The user, said for a person.
fn who(user: ContainerUser) -> String {
    match user.docker_user() {
        Some(u) => format!("runs as {u}"),
        None => "runs as the image's own user".into(),
    }
}

const NAME: &str = "container user";

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_platform::{ContainerHostAccess, FakePlatform};
    use darkmux_types::config_access::Source;

    fn platform(user: ContainerUser) -> FakePlatform {
        FakePlatform { container_host_access: ContainerHostAccess::Builtin, container_user: user }
    }

    #[test]
    fn an_override_that_does_not_parse_fails_naming_the_setting_and_the_fix() {
        let c = describe(Some(("1000:", Source::Config)), &platform(ContainerUser::Image));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("runtime.container_user") && c.message.contains("`1000:`"), "{}", c.message);
        let hint = c.hint.unwrap_or_default();
        assert!(hint.contains("darkmux config set runtime.container_user"), "{hint}");
    }

    #[test]
    fn unset_names_the_platforms_answer() {
        let image = describe(None, &platform(ContainerUser::Image));
        assert_eq!(image.status, Status::Pass);
        assert!(image.message.contains("the image's own user") && image.message.contains("fake"), "{}", image.message);
        let host = describe(None, &platform(ContainerUser::Host { uid: 1000, gid: Some(1000) }));
        assert!(host.message.contains("1000:1000"), "{}", host.message);
    }

    #[test]
    /// (#3276 re-review F1) The row names the tier the value came from: an
    /// env value is never credited to the config key.
    #[test]
    fn the_row_names_the_tier_the_value_came_from() {
        let image = platform(ContainerUser::Image);
        let env = describe(Some(("0:0", Source::Env)), &image);
        assert!(env.message.contains("DARKMUX_CONTAINER_USER") && !env.message.contains("runtime.container_user"), "{}", env.message);
        let bad_env = describe(Some(("x", Source::Env)), &image);
        assert_eq!(bad_env.status, Status::Fail);
        assert!(bad_env.message.starts_with("DARKMUX_CONTAINER_USER is `x`"), "{}", bad_env.message);
        let cfg = describe(Some(("0:0", Source::Config)), &image);
        assert!(cfg.message.contains("runtime.container_user") && !cfg.message.contains("DARKMUX_CONTAINER_USER"), "{}", cfg.message);
    }

    #[test]
    fn a_valid_override_says_it_overrides_the_platform() {
        let c = describe(Some(("0:0", Source::Config)), &platform(ContainerUser::Image));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("0:0") && c.message.contains("runtime.container_user"), "{}", c.message);
    }
}
