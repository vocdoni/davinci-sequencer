/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** `1` builds a bundle that always runs the demo network. */
  readonly VITE_DEMO?: string
  /** Shown in the footer; the image tag or commit in CI builds. */
  readonly VITE_BUILD_VERSION?: string
}

interface ImportMeta {
  readonly env: ImportMetaEnv
}
