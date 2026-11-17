// How the service treats an account. Review accounts (the App Store and Play reviewers' logins) are hidden from other
// people: they never appear to friends and get no friend-facing sender details on notifications.

/** `normal` is an ordinary account; `review` is a review account. */
export type Visibility = "normal" | "review";

/**
 * Reads the `toddgriffin_visibility` claim (ID token or userinfo) or a webhook's `visibility` field. Anything but
 * `"review"`, an absent value included, is `"normal"`.
 */
export function visibilityOf(value: unknown): Visibility {
  return value === "review" ? "review" : "normal";
}

/** Whether `subject` (verified ID token claims) is a review account. */
export function isReview(subject: { visibility?: Visibility }): boolean {
  return subject.visibility === "review";
}
