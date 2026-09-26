//! (#2923) Which darkmux runtime image a dispatch runs, and the check that it
//! was built for THIS darkmux.
//!
//! The host binary and the runtime inside the container speak one interface
//! (argv flags, the `/darkmux-out` layout, the trajectory shapes). A runtime
//! built for a different darkmux either dies at arg-parse (`unknown flag:
//! --session-id`, the #975 class) or, worse, runs and behaves differently.
//! Before #2923 the resolver preferred a local `darkmux-runtime:latest` on
//! PRESENCE alone, so a weeks-old unlabeled dev build silently shadowed the
//! version-pinned `ghcr.io/kstrat2001/darkmux-runtime:<version>`.
//!
//! **How an image's version is known, without running it.** The runtime
//! Dockerfile stamps `org.opencontainers.image.version` from the
//! `DARKMUX_VERSION` build-arg (#1461), and the publish workflow passes it for
//! every GHCR tag. That label is read with `docker image inspect`: metadata,
//! no container. Running `darkmux-runtime --version` would not help anyway: it
//! reports the runtime crate's own version, not darkmux's.
//!
//! **When an image carries no label**, the fallback is:
//!   * a GHCR ref whose tag is a version (`…/darkmux-runtime:3.13.0`): the tag
//!     IS the version, because the publish workflow tags by darkmux version
//!     (#759). That is the contract the pull-on-demand pin already relies on.
//!   * anything else (a local `docker build` with no build-arg, a retag like
//!     `darkmux-runtime:stale-pre-3.13`, GHCR `:latest`): the version is
//!     UNKNOWN, and unknown is treated as "not this darkmux". The default path
//!     skips it for the version-pinned image; an explicitly named one is
//!     refused with the rebuild command. Never run a runtime whose version
//!     cannot be established. That is exactly the image that bit the Studio.
//!
//! **What the check cannot see.** A version number names a release, not a
//! build: two dev builds that share `CARGO_PKG_VERSION` (a feature branch that
//! has not bumped it yet) look identical here. The label check makes skew
//! between RELEASES impossible to miss; within one version number it relies
//! on the version being bumped when the interface changes.
//!
//! Everything that decides is pure over an [`ImageInspector`], so the
//! decisions are unit-tested with a fake; [`DockerImageInspector`] is the one
//! implementation that shells out.

use anyhow::{bail, Result};
use std::process::Command;

/// Local repository name a source checkout builds the runtime under.
pub const RUNTIME_IMAGE_REPO: &str = "darkmux-runtime";

/// The local dev tag the default path considers first.
pub const RUNTIME_IMAGE: &str = "darkmux-runtime:latest";

/// GHCR repository for the published runtime image (#759).
pub const RUNTIME_IMAGE_GHCR_REPO: &str = "ghcr.io/kstrat2001/darkmux-runtime";

/// The OCI label the runtime Dockerfile stamps the darkmux version into.
pub const RUNTIME_IMAGE_VERSION_LABEL: &str = "org.opencontainers.image.version";

/// The version-pinned GHCR ref for a given darkmux version.
pub fn pinned_runtime_image(host_version: &str) -> String {
    format!("{RUNTIME_IMAGE_GHCR_REPO}:{host_version}")
}

/// What `docker image inspect` could say about one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageInspection {
    /// Not present locally (or the inspect failed; the caller has already
    /// confirmed the daemon answers, so this is "no such image").
    Absent,
    /// Present, with the version label if it carries a non-empty one.
    Present { version_label: Option<String> },
}

/// The seam: the only question the resolver asks Docker.
pub trait ImageInspector {
    fn inspect(&self, image: &str) -> ImageInspection;
}

/// The real inspector: `docker image inspect --format <label> -- <ref>`.
pub struct DockerImageInspector;

impl ImageInspector for DockerImageInspector {
    fn inspect(&self, image: &str) -> ImageInspection {
        let format = format!("{{{{index .Config.Labels \"{RUNTIME_IMAGE_VERSION_LABEL}\"}}}}");
        match Command::new("docker")
            .args(["image", "inspect", "--format", &format, "--", image])
            .output()
        {
            Ok(out) if out.status.success() => ImageInspection::Present {
                version_label: parse_version_label(&String::from_utf8_lossy(&out.stdout)),
            },
            _ => ImageInspection::Absent,
        }
    }
}

/// Parse the inspect template's output. Docker prints an empty line for a
/// null label map and `<no value>` for a missing key in a non-null one; both
/// mean "unlabeled".
pub fn parse_version_label(stdout: &str) -> Option<String> {
    let s = stdout.trim();
    (!s.is_empty() && s != "<no value>").then(|| s.to_string())
}

/// Split an image ref into (repository, tag). A digest ref (`repo@sha256:…`)
/// has no tag. The tag separator is the last `:` AFTER the last `/`, so a
/// registry port (`localhost:5000/repo`) is not mistaken for one.
pub fn repository_and_tag(image: &str) -> (&str, Option<&str>) {
    let name = image.split('@').next().unwrap_or(image);
    let slash = name.rfind('/').map_or(0, |i| i + 1);
    match name[slash..].rfind(':') {
        Some(i) => (&name[..slash + i], Some(&name[slash + i + 1..])),
        None => (name, None),
    }
}

/// True if `image` names one of darkmux's own runtime images: the local
/// `darkmux-runtime` repository (ANY tag, e.g. `darkmux-runtime:4.0-rc`) or
/// the GHCR repository. Such images have the runtime baked in and run
/// directly; everything else is an operator environment (#703) that gets the
/// runtime binary injected.
pub fn is_darkmux_runtime_ref(image: &str) -> bool {
    let (repo, _) = repository_and_tag(image);
    repo == RUNTIME_IMAGE_REPO || repo == RUNTIME_IMAGE_GHCR_REPO
}

/// The darkmux version `image` was built for, as far as can be known without
/// running it: the label when present, else a GHCR version tag (see the
/// module doc for why only GHCR's tag is trusted).
pub fn known_image_version(image: &str, version_label: Option<&str>) -> Option<String> {
    if let Some(label) = version_label {
        return Some(label.to_string());
    }
    let (repo, tag) = repository_and_tag(image);
    match tag {
        Some(t) if repo == RUNTIME_IMAGE_GHCR_REPO && t != "latest" => Some(t.to_string()),
        _ => None,
    }
}

/// Whether a present image can run under this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageVerdict {
    Matches,
    /// Built for a different darkmux.
    Mismatch { built_for: String },
    /// No version could be established.
    Unknown,
}

pub fn image_verdict(image: &str, version_label: Option<&str>, host_version: &str) -> ImageVerdict {
    match known_image_version(image, version_label) {
        Some(v) if v == host_version => ImageVerdict::Matches,
        Some(v) => ImageVerdict::Mismatch { built_for: v },
        None => ImageVerdict::Unknown,
    }
}

/// "was built for darkmux X" / "carries no version label …" — the clause
/// every message about a non-matching image uses.
pub fn describe_non_match(image: &str, verdict: &ImageVerdict) -> String {
    match verdict {
        ImageVerdict::Mismatch { built_for } => {
            format!("`{image}` was built for darkmux {built_for}")
        }
        _ => format!(
            "`{image}` carries no version label ({RUNTIME_IMAGE_VERSION_LABEL}), so darkmux \
             cannot tell which darkmux it was built for"
        ),
    }
}

/// The exact rebuild command for a local tag. A GHCR ref is never rebuilt
/// locally, and a digest ref has no tag to rebuild under, so both fall back
/// to the dev tag.
pub fn rebuild_command(image: &str, host_version: &str) -> String {
    let (repo, tag) = repository_and_tag(image);
    let target = match tag {
        Some(t) if repo == RUNTIME_IMAGE_REPO => format!("{repo}:{t}"),
        _ => RUNTIME_IMAGE.to_string(),
    };
    format!(
        "docker build --build-arg DARKMUX_VERSION={host_version} -t {target} runtime/"
    )
}

/// The refusal for an image that does not match this host.
pub fn mismatch_refusal(image: &str, verdict: &ImageVerdict, host_version: &str) -> String {
    let pinned = pinned_runtime_image(host_version);
    format!(
        "refusing to dispatch: runtime image {}; this darkmux is {host_version}. A runtime \
         built for another darkmux fails mid-dispatch (`unknown flag: …`) or behaves \
         differently, so it is not run (#2923).\n\
         Fix, one of:\n  \
         - use the version-pinned image: `docker pull {pinned}`, and name it (or drop \
         `--image`)\n  \
         - rebuild from a darkmux {host_version} source checkout: `{}`",
        describe_non_match(image, verdict),
        rebuild_command(image, host_version),
    )
}

/// The stderr notice when the default path skips a local `:latest`.
pub fn skipped_latest_notice(verdict: &ImageVerdict, host_version: &str) -> String {
    format!(
        "darkmux dispatch: local {}; this darkmux is {host_version} — not using it; \
         running the version-pinned `{}` instead (#2923). Rebuild it with `{}`, or remove it \
         with `docker rmi {RUNTIME_IMAGE}`.",
        describe_non_match(RUNTIME_IMAGE, verdict),
        pinned_runtime_image(host_version),
        rebuild_command(RUNTIME_IMAGE, host_version),
    )
}

/// Where the default (no `--image`, or injection-source) image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultImagePlan {
    /// The ref to run.
    pub image: String,
    /// The ref is the pinned GHCR image and is not present yet.
    pub needs_pull: bool,
    /// Set when a local `:latest` was present but skipped — the notice to
    /// print, so the operator learns why their local build was not used.
    pub skipped_latest: Option<String>,
}

/// Decide the default image. Prefers a local `:latest` ONLY when its version
/// matches the host; otherwise the version-pinned GHCR image (present, or to
/// be pulled). A present pinned image whose label contradicts its tag is
/// refused rather than trusted.
pub fn plan_default_image(
    inspector: &dyn ImageInspector,
    host_version: &str,
) -> Result<DefaultImagePlan> {
    let mut skipped_latest = None;
    if let ImageInspection::Present { version_label } = inspector.inspect(RUNTIME_IMAGE) {
        match image_verdict(RUNTIME_IMAGE, version_label.as_deref(), host_version) {
            ImageVerdict::Matches => {
                return Ok(DefaultImagePlan {
                    image: RUNTIME_IMAGE.to_string(),
                    needs_pull: false,
                    skipped_latest: None,
                });
            }
            other => skipped_latest = Some(skipped_latest_notice(&other, host_version)),
        }
    }
    let pinned = pinned_runtime_image(host_version);
    match inspector.inspect(&pinned) {
        ImageInspection::Absent => Ok(DefaultImagePlan {
            image: pinned,
            needs_pull: true,
            skipped_latest,
        }),
        ImageInspection::Present { version_label } => {
            match image_verdict(&pinned, version_label.as_deref(), host_version) {
                ImageVerdict::Matches => Ok(DefaultImagePlan {
                    image: pinned,
                    needs_pull: false,
                    skipped_latest,
                }),
                other => bail!(mismatch_refusal(&pinned, &other, host_version)),
            }
        }
    }
}

/// Resolve the default image end to end: plan, print the skip notice, pull
/// the pinned image when absent, and re-verify what the pull produced. `pull`
/// and `notice` are seams so the whole sequence is testable without Docker.
pub fn resolve_default_image(
    inspector: &dyn ImageInspector,
    host_version: &str,
    pull: &dyn Fn(&str) -> Result<()>,
    notice: &dyn Fn(&str),
) -> Result<String> {
    let plan = plan_default_image(inspector, host_version)?;
    if let Some(msg) = &plan.skipped_latest {
        notice(msg);
    }
    if plan.needs_pull {
        if let Err(e) = pull(&plan.image) {
            // The pull was the only matching runtime on offer. When a local
            // `:latest` was skipped to get here, say so in the error: the
            // operator is looking at a present image and a failed dispatch.
            return Err(match &plan.skipped_latest {
                Some(skipped) => e.context(format!(
                    "no runtime image for darkmux {host_version} is available: the local one \
                     was skipped and the pull failed.\n{skipped}"
                )),
                None => e,
            });
        }
        verify_present_image(inspector, &plan.image, host_version)?;
    }
    Ok(plan.image)
}

/// Refuse unless `image` is present AND matches the host.
pub fn verify_present_image(
    inspector: &dyn ImageInspector,
    image: &str,
    host_version: &str,
) -> Result<()> {
    match inspector.inspect(image) {
        ImageInspection::Absent => bail!(
            "runtime image `{image}` is not present locally, and darkmux does not run a runtime \
             image it cannot check first (#2923). Pull the version-pinned one with `docker pull \
             {}`, or build one with `{}`.",
            pinned_runtime_image(host_version),
            rebuild_command(image, host_version),
        ),
        ImageInspection::Present { version_label } => {
            match image_verdict(image, version_label.as_deref(), host_version) {
                ImageVerdict::Matches => Ok(()),
                other => bail!(mismatch_refusal(image, &other, host_version)),
            }
        }
    }
}

/// Resolve an explicitly named darkmux runtime image (`--image
/// darkmux-runtime:4.0-rc`, `--image ghcr.io/…:<v>`). The pinned ref for this
/// host is pulled when absent, exactly as the default path would; any other
/// explicit ref must already be present and match.
pub fn resolve_explicit_image(
    inspector: &dyn ImageInspector,
    image: &str,
    host_version: &str,
    pull: &dyn Fn(&str) -> Result<()>,
) -> Result<String> {
    if image == pinned_runtime_image(host_version)
        && inspector.inspect(image) == ImageInspection::Absent
    {
        pull(image)?;
    }
    verify_present_image(inspector, image, host_version)?;
    Ok(image.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A fake Docker image store: ref -> label (None = present, unlabeled).
    /// `pull` inserts what the registry would serve for a ref.
    #[derive(Default)]
    struct FakeImages {
        present: RefCell<HashMap<String, Option<String>>>,
        registry: HashMap<String, Option<String>>,
        pulls: RefCell<Vec<String>>,
    }

    impl FakeImages {
        fn with(mut self, image: &str, label: Option<&str>) -> Self {
            self.present
                .get_mut()
                .insert(image.to_string(), label.map(String::from));
            self
        }
        fn serving(mut self, image: &str, label: Option<&str>) -> Self {
            self.registry.insert(image.to_string(), label.map(String::from));
            self
        }
        fn pull(&self, image: &str) -> Result<()> {
            self.pulls.borrow_mut().push(image.to_string());
            match self.registry.get(image) {
                Some(label) => {
                    self.present
                        .borrow_mut()
                        .insert(image.to_string(), label.clone());
                    Ok(())
                }
                None => bail!("failed to pull the runtime image `{image}`"),
            }
        }
    }

    impl ImageInspector for FakeImages {
        fn inspect(&self, image: &str) -> ImageInspection {
            match self.present.borrow().get(image) {
                Some(label) => ImageInspection::Present {
                    version_label: label.clone(),
                },
                None => ImageInspection::Absent,
            }
        }
    }

    const HOST: &str = "3.13.0";

    fn pinned() -> String {
        pinned_runtime_image(HOST)
    }

    fn resolve(f: &FakeImages) -> (Result<String>, Vec<String>) {
        let notices = RefCell::new(Vec::new());
        let r = resolve_default_image(f, HOST, &|i| f.pull(i), &|m| {
            notices.borrow_mut().push(m.to_string())
        });
        (r, notices.into_inner())
    }

    // ── the #2923 regression: the Studio's shape ──────────────────────────

    #[test]
    fn an_unlabeled_local_latest_does_not_shadow_the_pinned_image() {
        // A 3-week-old `docker build` with no build-arg, next to the pinned
        // image for this host. Before #2923 the local tag won on presence.
        let f = FakeImages::default()
            .with(RUNTIME_IMAGE, None)
            .with(&pinned(), Some(HOST));
        let (r, notices) = resolve(&f);
        assert_eq!(r.unwrap(), pinned());
        assert!(f.pulls.borrow().is_empty(), "the pinned image was present");
        assert_eq!(notices.len(), 1, "skipping a present local build is said out loud");
        assert!(notices[0].contains("no version label"), "{}", notices[0]);
        assert!(notices[0].contains(HOST), "{}", notices[0]);
    }

    #[test]
    fn an_older_labeled_local_latest_is_skipped_naming_both_versions() {
        let f = FakeImages::default()
            .with(RUNTIME_IMAGE, Some("3.12.0"))
            .with(&pinned(), Some(HOST));
        let (r, notices) = resolve(&f);
        assert_eq!(r.unwrap(), pinned());
        assert!(notices[0].contains("3.12.0") && notices[0].contains(HOST), "{}", notices[0]);
        assert!(
            notices[0].contains(&format!("--build-arg DARKMUX_VERSION={HOST}")),
            "{}",
            notices[0]
        );
    }

    #[test]
    fn a_matching_local_latest_is_preferred_as_before() {
        // The dev workflow with a stamped build keeps working, and stays
        // quiet.
        let f = FakeImages::default()
            .with(RUNTIME_IMAGE, Some(HOST))
            .with(&pinned(), Some(HOST));
        let (r, notices) = resolve(&f);
        assert_eq!(r.unwrap(), RUNTIME_IMAGE);
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[test]
    fn a_skipped_local_latest_and_an_absent_pin_pulls_the_pin() {
        let f = FakeImages::default()
            .with(RUNTIME_IMAGE, None)
            .serving(&pinned(), Some(HOST));
        let (r, _) = resolve(&f);
        assert_eq!(r.unwrap(), pinned());
        assert_eq!(*f.pulls.borrow(), vec![pinned()]);
    }

    #[test]
    fn a_failed_pull_after_a_skip_refuses_naming_both_versions_and_the_fix() {
        // Only the mismatched image is available: refuse before running.
        let f = FakeImages::default().with(RUNTIME_IMAGE, Some("3.12.0"));
        let (r, _) = resolve(&f);
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("3.12.0"), "{msg}");
        assert!(msg.contains(HOST), "{msg}");
        assert!(msg.contains(&format!("DARKMUX_VERSION={HOST}")), "{msg}");
        assert!(msg.contains("failed to pull"), "the pull's own cause survives: {msg}");
    }

    #[test]
    fn nothing_local_pulls_the_pin_without_a_notice() {
        let f = FakeImages::default().serving(&pinned(), Some(HOST));
        let (r, notices) = resolve(&f);
        assert_eq!(r.unwrap(), pinned());
        assert!(notices.is_empty());
    }

    #[test]
    fn a_pulled_image_is_verified_before_it_runs() {
        // A registry serving a pinned tag whose label names another version
        // (a mistagged publish) is refused, not run.
        let f = FakeImages::default().serving(&pinned(), Some("3.12.0"));
        let (r, _) = resolve(&f);
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("refusing to dispatch"), "{msg}");
        assert!(msg.contains("3.12.0"), "{msg}");
    }

    #[test]
    fn a_present_pin_whose_label_contradicts_its_tag_is_refused() {
        let f = FakeImages::default().with(&pinned(), Some("3.12.0"));
        let msg = plan_default_image(&f, HOST).unwrap_err().to_string();
        assert!(msg.contains("3.12.0") && msg.contains(HOST), "{msg}");
    }

    #[test]
    fn an_unlabeled_pin_is_known_by_its_tag() {
        // The publish workflow tags by darkmux version (#759): for the GHCR
        // repo, the tag is the version when no label says otherwise.
        let f = FakeImages::default().with(&pinned(), None);
        assert_eq!(plan_default_image(&f, HOST).unwrap().image, pinned());
    }

    // ── explicitly named darkmux images ───────────────────────────────────

    fn explicit(f: &FakeImages, image: &str) -> Result<String> {
        resolve_explicit_image(f, image, HOST, &|i| f.pull(i))
    }

    #[test]
    fn an_explicit_rc_tag_that_matches_runs() {
        let f = FakeImages::default().with("darkmux-runtime:4.0-rc", Some(HOST));
        assert_eq!(explicit(&f, "darkmux-runtime:4.0-rc").unwrap(), "darkmux-runtime:4.0-rc");
    }

    #[test]
    fn an_explicit_unlabeled_rc_tag_is_refused_with_the_rebuild_for_that_tag() {
        let f = FakeImages::default().with("darkmux-runtime:4.0-rc", None);
        let msg = explicit(&f, "darkmux-runtime:4.0-rc").unwrap_err().to_string();
        assert!(msg.contains("refusing to dispatch"), "{msg}");
        assert!(msg.contains("`darkmux-runtime:4.0-rc`"), "names the image: {msg}");
        assert!(msg.contains("no version label"), "{msg}");
        assert!(
            msg.contains(&format!(
                "docker build --build-arg DARKMUX_VERSION={HOST} -t darkmux-runtime:4.0-rc runtime/"
            )),
            "{msg}"
        );
        assert!(msg.contains(&format!("docker pull {}", pinned())), "{msg}");
    }

    #[test]
    fn an_explicit_mismatched_tag_is_refused_naming_both_versions() {
        let f = FakeImages::default().with("darkmux-runtime:latest", Some("3.12.0"));
        let msg = explicit(&f, "darkmux-runtime:latest").unwrap_err().to_string();
        assert!(msg.contains("3.12.0") && msg.contains(HOST), "{msg}");
    }

    #[test]
    fn an_explicit_older_ghcr_tag_is_refused_by_its_tag() {
        let f = FakeImages::default().with("ghcr.io/kstrat2001/darkmux-runtime:3.9.0", None);
        let msg = explicit(&f, "ghcr.io/kstrat2001/darkmux-runtime:3.9.0")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("built for darkmux 3.9.0"), "{msg}");
    }

    #[test]
    fn an_explicit_absent_non_pinned_image_is_refused_not_pulled() {
        let f = FakeImages::default().serving("darkmux-runtime:4.0-rc", Some(HOST));
        let msg = explicit(&f, "darkmux-runtime:4.0-rc").unwrap_err().to_string();
        assert!(msg.contains("not present locally"), "{msg}");
        assert!(f.pulls.borrow().is_empty(), "never pull an unverifiable ref");
    }

    #[test]
    fn an_explicit_absent_pin_is_pulled_then_verified() {
        let f = FakeImages::default().serving(&pinned(), Some(HOST));
        assert_eq!(explicit(&f, &pinned()).unwrap(), pinned());
        assert_eq!(*f.pulls.borrow(), vec![pinned()]);
    }

    // ── ref parsing ───────────────────────────────────────────────────────

    #[test]
    fn darkmux_runtime_refs_are_recognized_by_repository_not_by_one_tag() {
        assert!(is_darkmux_runtime_ref(RUNTIME_IMAGE));
        assert!(is_darkmux_runtime_ref("darkmux-runtime:4.0-rc"));
        assert!(is_darkmux_runtime_ref("darkmux-runtime"));
        assert!(is_darkmux_runtime_ref("darkmux-runtime@sha256:abc"));
        assert!(is_darkmux_runtime_ref(&pinned()));
        assert!(!is_darkmux_runtime_ref("darkmux-runtime-bun:local"));
        assert!(!is_darkmux_runtime_ref("ghcr.io/kstrat2001/darkmux-runtime-evil:latest"));
        assert!(!is_darkmux_runtime_ref("rust:slim"));
        assert!(!is_darkmux_runtime_ref("someone/darkmux-runtime:latest"));
    }

    #[test]
    fn a_registry_port_is_not_a_tag() {
        assert_eq!(
            repository_and_tag("localhost:5000/darkmux-runtime"),
            ("localhost:5000/darkmux-runtime", None)
        );
        assert_eq!(
            repository_and_tag("localhost:5000/darkmux-runtime:x"),
            ("localhost:5000/darkmux-runtime", Some("x"))
        );
    }

    #[test]
    fn only_a_ghcr_version_tag_stands_in_for_a_missing_label() {
        assert_eq!(known_image_version("darkmux-runtime:3.13.0", None), None);
        assert_eq!(
            known_image_version("ghcr.io/kstrat2001/darkmux-runtime:latest", None),
            None
        );
        assert_eq!(
            known_image_version("ghcr.io/kstrat2001/darkmux-runtime:3.13.0", None).as_deref(),
            Some("3.13.0")
        );
        // A label always wins over a tag.
        assert_eq!(
            known_image_version("ghcr.io/kstrat2001/darkmux-runtime:3.13.0", Some("3.12.0"))
                .as_deref(),
            Some("3.12.0")
        );
    }

    #[test]
    fn empty_and_no_value_labels_are_unlabeled() {
        assert_eq!(parse_version_label("\n"), None);
        assert_eq!(parse_version_label("<no value>\n"), None);
        assert_eq!(parse_version_label("3.13.0\n").as_deref(), Some("3.13.0"));
    }
}
