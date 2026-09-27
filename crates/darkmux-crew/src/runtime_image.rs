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
//! **When an image carries no label** its version is UNKNOWN, and unknown is
//! treated as "not this darkmux": the default path skips it for the
//! version-pinned image, and an explicitly named one is refused with the
//! rebuild command. Never run a runtime whose version cannot be established;
//! that is exactly the image that bit the Studio. A GHCR version tag on an
//! unlabeled image may EXPLAIN a mismatch (`…:3.9.0` under a 3.13.0 host
//! reads "built for darkmux 3.9.0"), but never declares a match: every image
//! the publish workflow has pushed since 2.0.0 carries the label, so an
//! unlabeled one at this host's tag was not produced by it.
//!
//! **What the check cannot see.** A version number names a release, not a
//! build: two builds that share `CARGO_PKG_VERSION` look identical here. A
//! development build that falls back to the RELEASE image for its version
//! says so on stderr ([`dev_build_notice`]), naming the command that builds a
//! matching one from the checkout.
//!
//! **Run by id.** Resolution returns the image's content id alongside its
//! ref, and the container runs by id, so a tag re-pointed between the check
//! and `docker run` (a concurrent `docker build -t darkmux-runtime:latest`)
//! cannot swap an unchecked image in.
//!
//! Everything that decides is pure over an [`ImageInspector`], so the
//! decisions are unit-tested with a fake; [`DockerImageInspector`] is the one
//! implementation that shells out, bounded by [`INSPECT_TIMEOUT`].

use anyhow::{bail, Result};
use std::process::Command;
use std::time::Duration;

/// Local repository name a source checkout builds the runtime under.
pub const RUNTIME_IMAGE_REPO: &str = "darkmux-runtime";

/// The local dev tag the default path considers first.
pub const RUNTIME_IMAGE: &str = "darkmux-runtime:latest";

/// GHCR repository for the published runtime image (#759).
pub const RUNTIME_IMAGE_GHCR_REPO: &str = "ghcr.io/kstrat2001/darkmux-runtime";

/// The OCI label the runtime Dockerfile stamps the darkmux version into.
pub const RUNTIME_IMAGE_VERSION_LABEL: &str = "org.opencontainers.image.version";

/// Bound on one `docker image inspect`. A local metadata read answers in
/// milliseconds; a wedged daemon must fail the dispatch, not hang it.
pub const INSPECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The version-pinned GHCR ref for a given darkmux version.
pub fn pinned_runtime_image(host_version: &str) -> String {
    format!("{RUNTIME_IMAGE_GHCR_REPO}:{host_version}")
}

/// `Some(build string)` when this binary is a development build (a git build:
/// `darkmux_types::build_version()` carries a SHA tag), `None` for a release
/// or a source-tarball build. Only a development build can be newer than the
/// release image of its own version number.
pub fn host_dev_build() -> Option<String> {
    dev_build_from(&darkmux_types::build_version(), env!("CARGO_PKG_VERSION"))
}

fn dev_build_from(build: &str, version: &str) -> Option<String> {
    (build != version && !build.ends_with("(release)")).then(|| build.to_string())
}

/// What `docker image inspect` could say about one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageInspection {
    /// Not present locally.
    Absent,
    /// Present: its content id, and the version label if it carries one.
    Present {
        id: String,
        version_label: Option<String>,
    },
    /// The inspect could not answer (timed out, could not start). Nothing is
    /// known, so nothing may run.
    Unreadable(String),
}

/// The seam: the only question the resolver asks Docker.
pub trait ImageInspector {
    fn inspect(&self, image: &str) -> ImageInspection;
}

/// The real inspector: one bounded `docker image inspect --format
/// '{{.Id}}|<label>' -- <ref>`.
pub struct DockerImageInspector {
    pub timeout: Duration,
}

impl Default for DockerImageInspector {
    fn default() -> Self {
        Self {
            timeout: INSPECT_TIMEOUT,
        }
    }
}

impl ImageInspector for DockerImageInspector {
    fn inspect(&self, image: &str) -> ImageInspection {
        use crate::bounded_command::{run_bounded, Bounded};
        let format =
            format!("{{{{.Id}}}}|{{{{index .Config.Labels \"{RUNTIME_IMAGE_VERSION_LABEL}\"}}}}");
        let mut cmd = Command::new("docker");
        cmd.args(["image", "inspect", "--format", &format, "--", image]);
        match run_bounded(cmd, self.timeout) {
            Bounded::Finished { success: true, stdout, .. } => {
                parse_inspect_line(&String::from_utf8_lossy(&stdout))
            }
            Bounded::Finished { .. } => ImageInspection::Absent,
            Bounded::TimedOut { seconds } => ImageInspection::Unreadable(format!(
                "`docker image inspect {image}` did not answer within {seconds}s"
            )),
            Bounded::Interrupted => {
                ImageInspection::Unreadable(format!("`docker image inspect {image}` was interrupted"))
            }
            Bounded::SpawnFailed(e) => {
                ImageInspection::Unreadable(format!("could not run `docker image inspect`: {e}"))
            }
        }
    }
}

/// Parse one `{{.Id}}|<label>` line. An answer with no id is not a usable
/// answer: nothing could be run by it.
pub fn parse_inspect_line(stdout: &str) -> ImageInspection {
    let line = stdout.trim();
    let (id, label) = line.split_once('|').unwrap_or((line, ""));
    let id = id.trim();
    if id.is_empty() {
        return ImageInspection::Unreadable(format!(
            "`docker image inspect` printed no image id (got {line:?})"
        ));
    }
    ImageInspection::Present {
        id: id.to_string(),
        version_label: parse_version_label(label),
    }
}

/// Parse the label template's output. Docker prints an empty line for a
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

/// Whether a present image can run under this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageVerdict {
    Matches,
    /// Built for a different darkmux.
    Mismatch { built_for: String },
    /// No version could be established.
    Unknown,
}

/// Only the label can declare a match. With no label, a GHCR version tag
/// that differs from the host explains the mismatch; anything else is
/// unknown.
pub fn image_verdict(image: &str, version_label: Option<&str>, host_version: &str) -> ImageVerdict {
    match version_label {
        Some(v) if v == host_version => ImageVerdict::Matches,
        Some(v) => ImageVerdict::Mismatch { built_for: v.to_string() },
        None => match repository_and_tag(image) {
            (repo, Some(t)) if repo == RUNTIME_IMAGE_GHCR_REPO && t != "latest" && t != host_version => {
                ImageVerdict::Mismatch { built_for: t.to_string() }
            }
            _ => ImageVerdict::Unknown,
        },
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
        "docker build --build-arg DARKMUX_VERSION={host_version} -f runtime/Dockerfile -t {target} ."
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

/// The stderr notice when a DEVELOPMENT build is about to run the RELEASE
/// image for its version number: that image was built from the release
/// commit, and this binary was not.
pub fn dev_build_notice(dev_build: &str, host_version: &str) -> String {
    format!(
        "darkmux dispatch: this is a development build ({dev_build}), and it is running the \
         RELEASE runtime image `{}`, which may predate this source tree. To run a runtime built \
         from this checkout, run `{}` in it (#2923).",
        pinned_runtime_image(host_version),
        rebuild_command(RUNTIME_IMAGE, host_version),
    )
}

/// A checked image: the ref it was resolved under and the content id it
/// runs by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    pub reference: String,
    pub id: String,
}

/// Where the default (no `--image`, or injection-source) image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultImagePlan {
    /// The ref to run.
    pub image: String,
    /// The checked id, when the image is already present and matches.
    /// `None` exactly when `needs_pull`.
    pub id: Option<String>,
    /// The ref is the pinned GHCR image and is not present yet.
    pub needs_pull: bool,
    /// Set when a local `:latest` was present but skipped — the notice to
    /// print, so the operator learns why their local build was not used.
    pub skipped_latest: Option<String>,
    /// Set when a development build resolves to the release image.
    pub dev_build_note: Option<String>,
}

fn unreadable(image: &str, why: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "refusing to dispatch: could not check the runtime image `{image}`: {why}. darkmux does \
         not run a runtime image it cannot check (#2923); `darkmux doctor` reports Docker's \
         health."
    )
}

/// Decide the default image. Prefers a local `:latest` ONLY when its version
/// matches the host; otherwise the version-pinned GHCR image (present, or to
/// be pulled). A present pinned image that does not match (a contradicting
/// label, or none) is refused rather than trusted.
pub fn plan_default_image(
    inspector: &dyn ImageInspector,
    host_version: &str,
    dev_build: Option<&str>,
) -> Result<DefaultImagePlan> {
    let mut skipped_latest = None;
    match inspector.inspect(RUNTIME_IMAGE) {
        ImageInspection::Present { id, version_label } => {
            match image_verdict(RUNTIME_IMAGE, version_label.as_deref(), host_version) {
                ImageVerdict::Matches => {
                    return Ok(DefaultImagePlan {
                        image: RUNTIME_IMAGE.to_string(),
                        id: Some(id),
                        needs_pull: false,
                        skipped_latest: None,
                        dev_build_note: None,
                    });
                }
                other => skipped_latest = Some(skipped_latest_notice(&other, host_version)),
            }
        }
        ImageInspection::Unreadable(why) => return Err(unreadable(RUNTIME_IMAGE, &why)),
        ImageInspection::Absent => {}
    }
    let pinned = pinned_runtime_image(host_version);
    let dev_build_note = dev_build.map(|b| dev_build_notice(b, host_version));
    match inspector.inspect(&pinned) {
        ImageInspection::Absent => Ok(DefaultImagePlan {
            image: pinned,
            id: None,
            needs_pull: true,
            skipped_latest,
            dev_build_note,
        }),
        ImageInspection::Unreadable(why) => Err(unreadable(&pinned, &why)),
        ImageInspection::Present { id, version_label } => {
            match image_verdict(&pinned, version_label.as_deref(), host_version) {
                ImageVerdict::Matches => Ok(DefaultImagePlan {
                    image: pinned,
                    id: Some(id),
                    needs_pull: false,
                    skipped_latest,
                    dev_build_note,
                }),
                other => bail!(mismatch_refusal(&pinned, &other, host_version)),
            }
        }
    }
}

/// Resolve the default image end to end: plan, print the notices, pull the
/// pinned image when absent, and re-verify what the pull produced. `pull`
/// and `notice` are seams so the whole sequence is testable without Docker.
pub fn resolve_default_image(
    inspector: &dyn ImageInspector,
    host_version: &str,
    dev_build: Option<&str>,
    pull: &dyn Fn(&str) -> Result<()>,
    notice: &dyn Fn(&str),
) -> Result<ResolvedImage> {
    let plan = plan_default_image(inspector, host_version, dev_build)?;
    for msg in plan.skipped_latest.iter().chain(plan.dev_build_note.iter()) {
        notice(msg);
    }
    let id = match plan.id {
        Some(id) => id,
        None => {
            if let Err(e) = pull(&plan.image) {
                // The pull was the only matching runtime on offer. When a local
                // `:latest` was skipped to get here, say so in the error: the
                // operator is looking at a present image and a failed dispatch.
                return Err(match &plan.skipped_latest {
                    Some(skipped) => e.context(format!(
                        "no runtime image for darkmux {host_version} is available: the local \
                         one was skipped and the pull failed.\n{skipped}"
                    )),
                    None => e,
                });
            }
            verify_present_image(inspector, &plan.image, host_version)?
        }
    };
    Ok(ResolvedImage {
        reference: plan.image,
        id,
    })
}

/// Refuse unless `image` is present AND matches the host; returns its id.
pub fn verify_present_image(
    inspector: &dyn ImageInspector,
    image: &str,
    host_version: &str,
) -> Result<String> {
    match inspector.inspect(image) {
        ImageInspection::Absent => bail!(
            "runtime image `{image}` is not present locally, and darkmux does not run a runtime \
             image it cannot check first (#2923). Pull the version-pinned one with `docker pull \
             {}`, or build one with `{}`.",
            pinned_runtime_image(host_version),
            rebuild_command(image, host_version),
        ),
        ImageInspection::Unreadable(why) => Err(unreadable(image, &why)),
        ImageInspection::Present { id, version_label } => {
            match image_verdict(image, version_label.as_deref(), host_version) {
                ImageVerdict::Matches => Ok(id),
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
) -> Result<ResolvedImage> {
    if image == pinned_runtime_image(host_version)
        && inspector.inspect(image) == ImageInspection::Absent
    {
        pull(image)?;
    }
    let id = verify_present_image(inspector, image, host_version)?;
    Ok(ResolvedImage {
        reference: image.to_string(),
        id,
    })
}

/// Unit-test image store for `dispatch_internal`'s own tests, which drive
/// `dispatch()` end to end and must never reach the host's Docker: exactly
/// one `darkmux-runtime:latest`, labeled with this crate's version.
#[cfg(test)]
pub(crate) struct UnitTestMatchingLatest;

#[cfg(test)]
impl ImageInspector for UnitTestMatchingLatest {
    fn inspect(&self, image: &str) -> ImageInspection {
        if image == RUNTIME_IMAGE {
            ImageInspection::Present {
                id: "sha256:unit-test-runtime".into(),
                version_label: Some(env!("CARGO_PKG_VERSION").into()),
            }
        } else {
            ImageInspection::Absent
        }
    }
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
        unreadable: std::collections::HashSet<String>,
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
            if self.unreadable.contains(image) {
                return ImageInspection::Unreadable("timed out".into());
            }
            match self.present.borrow().get(image) {
                Some(label) => ImageInspection::Present {
                    id: fake_id(image),
                    version_label: label.clone(),
                },
                None => ImageInspection::Absent,
            }
        }
    }

    fn fake_id(image: &str) -> String {
        format!("sha256:id-of-{image}")
    }

    const HOST: &str = "3.13.0";

    fn pinned() -> String {
        pinned_runtime_image(HOST)
    }

    fn resolve_as(f: &FakeImages, dev_build: Option<&str>) -> (Result<String>, Vec<String>) {
        let notices = RefCell::new(Vec::new());
        let r = resolve_default_image(f, HOST, dev_build, &|i| f.pull(i), &|m| {
            notices.borrow_mut().push(m.to_string())
        });
        // Every resolution runs by the id it checked.
        if let Ok(ok) = &r {
            assert_eq!(ok.id, fake_id(&ok.reference), "runs by the checked id");
        }
        (r.map(|ok| ok.reference), notices.into_inner())
    }

    fn resolve(f: &FakeImages) -> (Result<String>, Vec<String>) {
        resolve_as(f, None)
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
        let msg = plan_default_image(&f, HOST, None).unwrap_err().to_string();
        assert!(msg.contains("3.12.0") && msg.contains(HOST), "{msg}");
    }

    #[test]
    fn an_unlabeled_image_at_the_pinned_tag_is_refused_not_trusted_by_its_tag() {
        // (#2923 review C8) Every published image carries the label, so an
        // unlabeled one at this host's tag was not produced by the publish
        // workflow. Its tag cannot vouch for it.
        let f = FakeImages::default().with(&pinned(), None);
        let msg = plan_default_image(&f, HOST, None).unwrap_err().to_string();
        assert!(msg.contains("no version label"), "{msg}");
    }

    #[test]
    fn a_development_build_is_told_it_runs_the_release_image() {
        // (#2923 review C2) A dev build shares its version number with the
        // release, so the release image passes the label check while possibly
        // predating the checkout. Say so, with the build that would match.
        let f = FakeImages::default().with(&pinned(), Some(HOST));
        let (r, notices) = resolve_as(&f, Some("3.13.0 (2c3ee0fdd✱)"));
        assert_eq!(r.unwrap(), pinned());
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("development build (3.13.0 (2c3ee0fdd✱))"), "{}", notices[0]);
        assert!(notices[0].contains("may predate this source tree"), "{}", notices[0]);
        assert!(
            notices[0].contains(&format!("--build-arg DARKMUX_VERSION={HOST} -f runtime/Dockerfile -t darkmux-runtime:latest .")),
            "{}",
            notices[0]
        );
    }

    #[test]
    fn a_development_build_with_its_own_matching_image_is_not_nagged() {
        let f = FakeImages::default()
            .with(RUNTIME_IMAGE, Some(HOST))
            .with(&pinned(), Some(HOST));
        let (r, notices) = resolve_as(&f, Some("3.13.0 (2c3ee0fdd)"));
        assert_eq!(r.unwrap(), RUNTIME_IMAGE);
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[test]
    fn only_a_git_build_counts_as_a_development_build() {
        assert_eq!(dev_build_from("3.13.0 (release)", "3.13.0"), None);
        assert_eq!(dev_build_from("3.13.0", "3.13.0"), None);
        assert_eq!(
            dev_build_from("3.13.0 (2c3ee0fdd✱)", "3.13.0").as_deref(),
            Some("3.13.0 (2c3ee0fdd✱)")
        );
    }

    #[test]
    fn this_binary_reports_its_own_build_string_as_a_development_build() {
        // The wrapper reads THIS binary's build: a git build (a tag other
        // than `release`) is a development build and reports its full build
        // string; a release or tagless build is not one.
        let build = darkmux_types::build_version();
        let tagged = build.len() > env!("CARGO_PKG_VERSION").len();
        let want = (tagged && !build.ends_with("(release)")).then(|| build.clone());
        assert_eq!(host_dev_build(), want, "build = {build}");
    }

    #[test]
    fn an_image_that_cannot_be_inspected_is_refused_and_nothing_is_pulled() {
        let mut f = FakeImages::default().serving(&pinned(), Some(HOST));
        f.unreadable.insert(RUNTIME_IMAGE.to_string());
        let (r, _) = resolve(&f);
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("could not check") && msg.contains("timed out"), "{msg}");
        assert!(f.pulls.borrow().is_empty(), "a wedged docker is not a reason to pull");
    }

    /// (#2923 review C7) A docker that never answers must not hang the
    /// dispatch: the inspect is bounded and reads as Unreadable.
    #[test]
    #[serial_test::serial] // mutates PATH
    fn a_docker_inspect_that_hangs_is_bounded_and_unreadable() {
        let dir = tempfile::TempDir::new().unwrap();
        let docker = dir.path().join("docker");
        std::fs::write(&docker, "#!/bin/sh\nexec sleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var("PATH").ok();
        // SAFETY: #[serial].
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", dir.path().display(), prev.clone().unwrap_or_default()))
        };
        let started = std::time::Instant::now();
        let got = DockerImageInspector {
            timeout: Duration::from_millis(300),
        }
        .inspect(RUNTIME_IMAGE);
        let took = started.elapsed();
        unsafe {
            match prev {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        assert!(matches!(got, ImageInspection::Unreadable(_)), "{got:?}");
        assert!(took < Duration::from_secs(10), "bounded, took {took:?}");
    }

    #[test]
    fn an_inspect_answer_with_no_id_is_not_usable() {
        assert!(matches!(parse_inspect_line("\n"), ImageInspection::Unreadable(_)));
        assert!(matches!(parse_inspect_line("|3.13.0\n"), ImageInspection::Unreadable(_)));
        assert_eq!(
            parse_inspect_line("sha256:abc|3.13.0\n"),
            ImageInspection::Present { id: "sha256:abc".into(), version_label: Some("3.13.0".into()) }
        );
        assert_eq!(
            parse_inspect_line("sha256:abc|\n"),
            ImageInspection::Present { id: "sha256:abc".into(), version_label: None }
        );
        assert_eq!(
            parse_inspect_line("sha256:abc|<no value>\n"),
            ImageInspection::Present { id: "sha256:abc".into(), version_label: None }
        );
    }

    // ── explicitly named darkmux images ───────────────────────────────────

    fn explicit(f: &FakeImages, image: &str) -> Result<String> {
        resolve_explicit_image(f, image, HOST, &|i| f.pull(i)).map(|r| {
            assert_eq!(r.id, fake_id(image), "runs by the checked id");
            r.reference
        })
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
                "docker build --build-arg DARKMUX_VERSION={HOST} -f runtime/Dockerfile -t darkmux-runtime:4.0-rc ."
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
    fn a_ghcr_tag_explains_a_mismatch_but_never_declares_a_match() {
        let v = |image: &str, label: Option<&str>| image_verdict(image, label, "3.13.0");
        // A local tag says nothing, whatever it spells.
        assert_eq!(v("darkmux-runtime:3.13.0", None), ImageVerdict::Unknown);
        assert_eq!(v("ghcr.io/kstrat2001/darkmux-runtime:latest", None), ImageVerdict::Unknown);
        // An unlabeled GHCR image at THIS version is unknown, not a match.
        assert_eq!(v("ghcr.io/kstrat2001/darkmux-runtime:3.13.0", None), ImageVerdict::Unknown);
        // At another version, the tag explains the mismatch.
        assert_eq!(
            v("ghcr.io/kstrat2001/darkmux-runtime:3.9.0", None),
            ImageVerdict::Mismatch { built_for: "3.9.0".into() }
        );
        // A label always wins over a tag.
        assert_eq!(
            v("ghcr.io/kstrat2001/darkmux-runtime:3.13.0", Some("3.12.0")),
            ImageVerdict::Mismatch { built_for: "3.12.0".into() }
        );
        assert_eq!(v("darkmux-runtime:anything", Some("3.13.0")), ImageVerdict::Matches);
    }

    #[test]
    fn empty_and_no_value_labels_are_unlabeled() {
        assert_eq!(parse_version_label("\n"), None);
        assert_eq!(parse_version_label("<no value>\n"), None);
        assert_eq!(parse_version_label("3.13.0\n").as_deref(), Some("3.13.0"));
    }
}
