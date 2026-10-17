//! Which WebKitGTK rendering mode the app runs in, read from the environment variables that
//! select it.

use std::ffi::OsString;

/// Names the effective rendering mode from the variables set once `main.rs` has applied its
/// defaults. A variable counts as set whatever its value, as `main.rs` treats it.
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
