// Reporting someone to Todd's review queue. A signed-in player is reported for by your server with
// `accounts.player(sub).report` (or `accounts.reportAsPlayer`); the older `friends.report` uses the person's own
// token and is deprecated. A guest has no token, so your server reports for them with `accounts.reportAsGuest` and your client
// credentials. Only accounts can be reported, never guests.

/** Why the person is being reported: their username (`name`), their profile picture (`picture`), or behaviour. */
export type Reason = "name" | "picture" | "cheating" | "harassment" | "other";

/** The service refuses notes longer than this many characters. */
export const MAX_NOTE_CHARS = 500;

/** A report your server files for a guest. */
export interface GuestReport {
  /** The account being reported. */
  sub: string;
  /** Why. */
  reason: Reason;
  /** What happened, in the guest's words (at most `MAX_NOTE_CHARS`). */
  note?: string;
  /** Your own stable id for the guest, so their reports are limited and deduplicated as one reporter. Without it, the IP is used. */
  guestId?: string;
  /** The guest's verified IP (the one your nginx passed on), kept with the report for abuse review. */
  guestIp?: string;
}

/** The JSON body for `POST /api/v1/reports`, leaving out what wasn't given. */
export function reportBody(report: GuestReport): Record<string, string> {
  const wire: Record<string, string> = { sub: report.sub, reason: report.reason };
  if (report.note !== undefined) wire.note = report.note;
  if (report.guestId !== undefined) wire.guest_id = report.guestId;
  if (report.guestIp !== undefined) wire.guest_ip = report.guestIp;
  return wire;
}

/** A report your server files for a signed-in player. */
export interface PlayerReport {
  /** The player reporting: a `sub` who signed in to your project. */
  reporterSub: string;
  /** The account being reported. */
  sub: string;
  /** Why. */
  reason: Reason;
  /** At most `MAX_NOTE_CHARS`. */
  note?: string;
}

/** The JSON body for `POST /api/v1/reports` with `reporter_sub`, leaving out what wasn't given. */
export function playerReportBody(report: PlayerReport): Record<string, string> {
  const wire: Record<string, string> = { reporter_sub: report.reporterSub, sub: report.sub, reason: report.reason };
  if (report.note !== undefined) wire.note = report.note;
  return wire;
}
