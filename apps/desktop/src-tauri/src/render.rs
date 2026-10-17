//! Which WebKitGTK rendering mode the app runs in: the default `main.rs` picks before Tauri
//! starts, and the mode the environment variables that select it give.

use std::ffi::OsString;
use std::fmt;

/// The variables that select a rendering mode. If any is set, the app sets none of its own.
pub const RENDER_VARS: [&str; 5] = [
    "WEBKIT_DISABLE_DMABUF_RENDERER",
    "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
    "__NV_DISABLE_EXPLICIT_SYNC",
    "WEBKIT_DISABLE_COMPOSITING_MODE",
    "WEBKIT_SKIA_ENABLE_CPU_RENDERING",
];

/// Present when the NVIDIA kernel driver is loaded.
const NVIDIA_VERSION_FILE: &str = "/proc/driver/nvidia/version";

/// What `main.rs` does to the rendering environment, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderChoice {
    /// You set these variables yourself, so the app sets nothing.
    UserSet(Vec<&'static str>),
    /// NVIDIA: GPU rendering with frames sent through shared memory. WebKit's default DMA-BUF
    /// path crashes with Wayland Error 71 there (WebKit bug 280210).
    GpuShm,
    /// No NVIDIA driver: WebKit's own default.
    WebkitDefault,
}

impl RenderChoice {
    /// The variable to set to `1`, if any.
    pub fn var_to_set(&self) -> Option<&'static str> {
        match self {
            RenderChoice::GpuShm => Some("WEBKIT_DMABUF_RENDERER_FORCE_SHM"),
            RenderChoice::UserSet(_) | RenderChoice::WebkitDefault => None,
        }
    }
}

impl fmt::Display for RenderChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderChoice::UserSet(vars) => write!(f, "user-set ({})", vars.join(", ")),
            RenderChoice::GpuShm => write!(
                f,
                "gpu-shm (NVIDIA driver found, set WEBKIT_DMABUF_RENDERER_FORCE_SHM=1)"
            ),
            RenderChoice::WebkitDefault => write!(f, "webkit-default (no NVIDIA driver found)"),
        }
    }
}

/// Picks the rendering default. Any variable in `RENDER_VARS` you set wins, whatever its value;
/// otherwise NVIDIA gets FORCE_SHM and every other GPU keeps WebKit's default.
pub fn choose_render_env(
    lookup_env: impl Fn(&str) -> Option<OsString>,
    nvidia_present: bool,
) -> RenderChoice {
    let user_set: Vec<_> = RENDER_VARS
        .into_iter()
        .filter(|name| lookup_env(name).is_some())
        .collect();
    if !user_set.is_empty() {
        RenderChoice::UserSet(user_set)
    } else if nvidia_present {
        RenderChoice::GpuShm
    } else {
        RenderChoice::WebkitDefault
    }
}

/// Chooses the default for this process and sets its variable. Call it before any thread starts:
/// setting the environment while another thread reads it is undefined behavior.
pub fn apply_render_env() -> RenderChoice {
    let nvidia_present = std::path::Path::new(NVIDIA_VERSION_FILE).exists();
    let choice = choose_render_env(|name| std::env::var_os(name), nvidia_present);
    if let Some(name) = choice.var_to_set() {
        std::env::set_var(name, "1");
    }
    choice
}

/// Names the effective rendering mode from the variables set once `main.rs` has applied its
/// default. A variable counts as set whatever its value, as `choose_render_env` treats it.
///
/// - `cpu`: `WEBKIT_DISABLE_DMABUF_RENDERER` or `WEBKIT_SKIA_ENABLE_CPU_RENDERING`
/// - `gpu-shm`: otherwise `WEBKIT_DMABUF_RENDERER_FORCE_SHM`
/// - `explicit-sync`: otherwise `__NV_DISABLE_EXPLICIT_SYNC`
/// - `no-compositing`: otherwise `WEBKIT_DISABLE_COMPOSITING_MODE`
/// - `webkit-default`: none of them
pub fn describe_render_env(lookup_env: impl Fn(&str) -> Option<OsString>) -> &'static str {
    let set = |name: &str| lookup_env(name).is_some();
    if set("WEBKIT_DISABLE_DMABUF_RENDERER") || set("WEBKIT_SKIA_ENABLE_CPU_RENDERING") {
        "cpu"
    } else if set("WEBKIT_DMABUF_RENDERER_FORCE_SHM") {
        "gpu-shm"
    } else if set("__NV_DISABLE_EXPLICIT_SYNC") {
        "explicit-sync"
    } else if set("WEBKIT_DISABLE_COMPOSITING_MODE") {
        "no-compositing"
    } else {
        "webkit-default"
    }
}

/// The mode for this process's environment.
pub fn current_render_mode() -> &'static str {
    describe_render_env(|name| std::env::var_os(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_with(vars: &[&str]) -> &'static str {
        describe_render_env(|name| vars.contains(&name).then(|| OsString::from("1")))
    }

    fn env_with(vars: &'static [&'static str]) -> impl Fn(&str) -> Option<OsString> {
        move |name| vars.contains(&name).then(|| OsString::from("1"))
    }

    #[test]
    fn nvidia_gets_force_shm() {
        let choice = choose_render_env(env_with(&[]), true);
        assert_eq!(choice, RenderChoice::GpuShm);
        assert_eq!(
            choice.var_to_set(),
            Some("WEBKIT_DMABUF_RENDERER_FORCE_SHM")
        );
        let applied = describe_render_env(|name| {
            (Some(name) == choice.var_to_set()).then(|| OsString::from("1"))
        });
        assert_eq!(applied, "gpu-shm");
    }

    #[test]
    fn other_gpus_keep_the_webkit_default() {
        let choice = choose_render_env(env_with(&[]), false);
        assert_eq!(choice, RenderChoice::WebkitDefault);
        assert_eq!(choice.var_to_set(), None);
    }

    #[test]
    fn each_user_set_variable_wins() {
        for var in RENDER_VARS {
            for nvidia in [true, false] {
                let choice =
                    choose_render_env(|name| (name == var).then(|| OsString::from("1")), nvidia);
                assert_eq!(
                    choice,
                    RenderChoice::UserSet(vec![var]),
                    "{var}, nvidia {nvidia}"
                );
                assert_eq!(choice.var_to_set(), None);
            }
        }
    }

    #[test]
    fn user_set_names_every_variable_set() {
        let choice = choose_render_env(
            env_with(&[
                "WEBKIT_SKIA_ENABLE_CPU_RENDERING",
                "__NV_DISABLE_EXPLICIT_SYNC",
            ]),
            true,
        );
        assert_eq!(
            choice.to_string(),
            "user-set (__NV_DISABLE_EXPLICIT_SYNC, WEBKIT_SKIA_ENABLE_CPU_RENDERING)"
        );
    }

    #[test]
    fn an_empty_user_value_still_wins() {
        let choice = choose_render_env(
            |name| (name == "WEBKIT_DISABLE_DMABUF_RENDERER").then(OsString::new),
            true,
        );
        assert_eq!(
            choice,
            RenderChoice::UserSet(vec!["WEBKIT_DISABLE_DMABUF_RENDERER"])
        );
    }

    #[test]
    fn unrelated_variables_do_not_count() {
        let choice = choose_render_env(
            env_with(&["WEBKIT_DEBUG", "__NV_PRIME_RENDER_OFFLOAD"]),
            true,
        );
        assert_eq!(choice, RenderChoice::GpuShm);
    }

    #[test]
    fn each_variable_maps_to_its_mode() {
        assert_eq!(mode_with(&["WEBKIT_DISABLE_DMABUF_RENDERER"]), "cpu");
        assert_eq!(mode_with(&["WEBKIT_SKIA_ENABLE_CPU_RENDERING"]), "cpu");
        assert_eq!(mode_with(&["WEBKIT_DMABUF_RENDERER_FORCE_SHM"]), "gpu-shm");
        assert_eq!(mode_with(&["__NV_DISABLE_EXPLICIT_SYNC"]), "explicit-sync");
    }

    #[test]
    fn disable_dmabuf_wins_over_the_others() {
        assert_eq!(
            mode_with(&[
                "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
                "__NV_DISABLE_EXPLICIT_SYNC",
                "WEBKIT_DISABLE_COMPOSITING_MODE",
                "WEBKIT_DISABLE_DMABUF_RENDERER",
            ]),
            "cpu"
        );
    }

    #[test]
    fn force_shm_wins_over_explicit_sync_and_compositing() {
        assert_eq!(
            mode_with(&[
                "__NV_DISABLE_EXPLICIT_SYNC",
                "WEBKIT_DISABLE_COMPOSITING_MODE",
                "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
            ]),
            "gpu-shm"
        );
    }

    #[test]
    fn explicit_sync_wins_over_compositing() {
        assert_eq!(
            mode_with(&[
                "WEBKIT_DISABLE_COMPOSITING_MODE",
                "__NV_DISABLE_EXPLICIT_SYNC"
            ]),
            "explicit-sync"
        );
    }

    #[test]
    fn skia_cpu_rendering_wins_over_the_gpu_modes() {
        assert_eq!(
            mode_with(&[
                "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
                "__NV_DISABLE_EXPLICIT_SYNC",
                "WEBKIT_SKIA_ENABLE_CPU_RENDERING",
            ]),
            "cpu"
        );
    }

    #[test]
    fn compositing_mode_alone_gives_no_compositing() {
        assert_eq!(
            mode_with(&["WEBKIT_DISABLE_COMPOSITING_MODE"]),
            "no-compositing"
        );
    }

    #[test]
    fn no_variable_gives_webkit_default() {
        assert_eq!(mode_with(&[]), "webkit-default");
    }

    #[test]
    fn an_empty_value_still_counts_as_set() {
        let mode = describe_render_env(|name| {
            (name == "WEBKIT_DISABLE_DMABUF_RENDERER").then(OsString::new)
        });
        assert_eq!(mode, "cpu");
    }
}
