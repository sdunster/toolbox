/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Required in a production build; see `lib/api.ts`. */
  readonly VITE_API_URL?: string;
  /** Stamped with the deploy commit SHA in CI; `"dev"` locally. */
  readonly VITE_CLIENT_VERSION?: string;
  /** Cloudflare Turnstile site key; unset disables it. See `lib/turnstile.ts`. */
  readonly VITE_TURNSTILE_SITE_KEY?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
