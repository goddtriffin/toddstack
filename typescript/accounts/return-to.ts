// Post-sign-in redirects that can't leave your site.

/**
 * `target` if it is a path on this site, else null. Refuses `//host`, `/\host`, absolute URLs, control
 * characters and anything over 1000 bytes: the shapes open-redirect attacks use.
 */
export function safeReturnTo(target: string): string | null {
  const ok = target.startsWith("/") &&
    !target.startsWith("//") &&
    !target.startsWith("/\\") &&
    // deno-lint-ignore no-control-regex
    !/[\u0000-\u001f\u007f-\u009f\\]/.test(target) &&
    new TextEncoder().encode(target).length < 1000;
  return ok ? target : null;
}
