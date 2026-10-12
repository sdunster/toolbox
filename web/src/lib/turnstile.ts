import { useCallback, useEffect, useRef, useState } from "react";

/**
 * Cloudflare Turnstile for the two unauthenticated, mail-sending mutations
 * (`requestAuthCode`, `requestSubmitCode`).
 *
 * Optional in the same way the API side is (`api/src/turnstile.rs`): with no
 * `VITE_TURNSTILE_SITE_KEY` in the build, no script loads, no widget renders,
 * and the forms send `turnstileToken: null` — which the API accepts as long as
 * it has no `TURNSTILE_SECRET_KEY` either. So a fork works with no Cloudflare
 * account, and a deployment turns it on by setting the site key here *before*
 * the secret key there (the other order rejects every code request).
 *
 * Loaded straight from Cloudflare rather than through an npm wrapper: the
 * script has to come from `challenges.cloudflare.com` regardless, and the
 * explicit-render API this needs is four calls.
 */

interface TurnstileRenderOptions {
  sitekey: string;
  appearance?: "always" | "execute" | "interaction-only";
  callback?: (token: string) => void;
  "expired-callback"?: () => void;
  "error-callback"?: () => void;
}

interface TurnstileApi {
  render(container: HTMLElement, options: TurnstileRenderOptions): string;
  reset(widgetId: string): void;
  remove(widgetId: string): void;
}

declare global {
  interface Window {
    turnstile?: TurnstileApi;
  }
}

const SCRIPT_SRC =
  "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit";

/** The configured site key, or `null` when Turnstile is off for this build. */
export function turnstileSiteKey(): string | null {
  return import.meta.env.VITE_TURNSTILE_SITE_KEY?.trim() || null;
}

let scriptPromise: Promise<TurnstileApi> | null = null;

function loadTurnstile(): Promise<TurnstileApi> {
  if (window.turnstile) return Promise.resolve(window.turnstile);
  scriptPromise ??= new Promise<TurnstileApi>((resolve, reject) => {
    const script = document.createElement("script");
    script.src = SCRIPT_SRC;
    script.onload = () => {
      if (window.turnstile) {
        resolve(window.turnstile);
      } else {
        reject(new Error("Turnstile script loaded without defining its API"));
      }
    };
    script.onerror = () => {
      // Let a later mount retry rather than caching the failure forever.
      scriptPromise = null;
      script.remove();
      reject(new Error("Failed to load the Turnstile script"));
    };
    document.head.appendChild(script);
  });
  return scriptPromise;
}

export interface TurnstileState {
  /** Whether this build has Turnstile turned on at all. */
  enabled: boolean;
  /**
   * The current unused token, `null` until the challenge passes (and always
   * `null` when disabled). Gate the send button on this when `enabled`.
   */
  token: string | null;
  /** The script or widget failed; show a message rather than a dead button. */
  failed: boolean;
  /** Attach to an always-mounted `<div>`; the widget renders into it. */
  mountWidget: (el: HTMLDivElement | null) => void;
  /**
   * Take the current token for a request and start a fresh challenge. A token
   * is single-use at siteverify, so a resend must never reuse the last one —
   * and since the API answers `true` regardless (anti-enumeration), a reused
   * token would fail silently: the user just never gets their code.
   */
  consume: () => string | null;
}

export function useTurnstile(): TurnstileState {
  const siteKey = turnstileSiteKey();
  const [container, setContainer] = useState<HTMLDivElement | null>(null);
  const [token, setToken] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const widgetIdRef = useRef<string | null>(null);

  useEffect(() => {
    if (!siteKey || !container) return;
    let cancelled = false;
    loadTurnstile().then(
      (api) => {
        if (cancelled) return;
        widgetIdRef.current = api.render(container, {
          sitekey: siteKey,
          // Invisible unless Cloudflare actually wants an interaction.
          appearance: "interaction-only",
          callback: (t) => {
            setFailed(false);
            setToken(t);
          },
          "expired-callback": () => setToken(null),
          "error-callback": () => {
            setToken(null);
            setFailed(true);
          },
        });
      },
      (err: unknown) => {
        if (cancelled) return;
        console.warn("[turnstile]", err);
        setFailed(true);
      },
    );
    return () => {
      cancelled = true;
      if (widgetIdRef.current !== null) {
        window.turnstile?.remove(widgetIdRef.current);
        widgetIdRef.current = null;
      }
    };
  }, [siteKey, container]);

  const consume = useCallback(() => {
    const current = token;
    if (siteKey && widgetIdRef.current !== null) {
      setToken(null);
      window.turnstile?.reset(widgetIdRef.current);
    }
    return current;
  }, [siteKey, token]);

  return {
    enabled: siteKey !== null,
    token,
    failed,
    mountWidget: setContainer,
    consume,
  };
}
