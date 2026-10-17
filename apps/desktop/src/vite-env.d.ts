/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Dev-only scroll benchmark: `1` runs the timing scenarios, `checks` the UI checks */
  readonly VITE_SCROLL_BENCH?: string;
}

// CSS Modules type shim
declare module '*.module.css' {
  const classes: { readonly [key: string]: string };
  export default classes;
}

// Image assets
declare module '*.svg' {
  const src: string;
  export default src;
}
declare module '*.png' {
  const src: string;
  export default src;
}
