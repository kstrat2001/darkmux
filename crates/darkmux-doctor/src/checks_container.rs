//! (#3275) Which user a dispatch container runs as: the operator's
//! `runtime.container_user` override, else the platform's answer. A value
//! that doesn't parse refuses every dispatch, so doctor fails it here first,
//! naming the setting and the fix.

use crate::{Check, Status};
use darkmux_platform::{ContainerUser, Platform};

pub fn check_container_user() -> Check {
    describe(darkmux_types::config_access::container_user_override().as_deref(), darkmux_platform::current())
}

/// The row for an override (`None` when unset) on `platform`.
fn describe(override_value: Option<&str>, platform: &dyn Platform) -> Check {
    let check = |status, message: String, hint: Option<String>| Check { name: NAME.into(), status, message, hint };
    let Some(value) = override_value else {
        return check(
            Status::Pass,
            format!("{} (the `{}` platform's answer; `runtime.container_user` unset)", who(platform.container_user()), platform.id()),
            None,
        );
    };
    match ContainerUser::parse(value) {
        Ok(user) => check(
            Status::Pass,
            format!("{} (`runtime.container_user` = `{value}`, over the `{}` platform's answer)", who(user), platform.id()),
            None,
        ),
        Err(why) => check(
            Status::Fail,
            format!("runtime.container_user (or DARKMUX_CONTAINER_USER) is `{value}`, which every dispatch refuses: {why}"),
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

    fn platform(user: ContainerUser) -> FakePlatform {
        FakePlatform { container_host_access: ContainerHostAccess::Builtin, container_user: user }
    }

    #[test]
    fn an_override_that_does_not_parse_fails_naming_the_setting_and_the_fix() {
        let c = describe(Some("1000:"), &platform(ContainerUser::Image));
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
    fn a_valid_override_says_it_overrides_the_platform() {
        let c = describe(Some("0:0"), &platform(ContainerUser::Image));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("0:0") && c.message.contains("runtime.container_user"), "{}", c.message);
    }
}
