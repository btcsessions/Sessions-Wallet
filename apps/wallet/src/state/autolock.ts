/** How long the app may sit in the background before it locks. */

/** Phones always lock after a minute; computers can choose. */
export const isComputer = !/Android|iPhone|iPad/i.test(typeof navigator === "undefined" ? "" : navigator.userAgent);

export const LOCK_CHOICES: { minutes: number; label: string }[] = [
  { minutes: 1, label: "1 minute" },
  { minutes: 15, label: "15 minutes" },
  { minutes: 60, label: "1 hour" },
  { minutes: 240, label: "4 hours" },
  { minutes: 0, label: "Only when I quit" },
];

const KEY = "sessions.autoLockMinutes";

/** Minutes in the background before locking; 0 = never (until the app quits). Per device. */
export function lockAfterMinutes(): number {
  if (!isComputer) return 1;
  try {
    const v = Number(localStorage.getItem(KEY));
    return LOCK_CHOICES.some((c) => c.minutes === v) && localStorage.getItem(KEY) !== null ? v : 1;
  } catch {
    return 1;
  }
}

export function setLockAfterMinutes(minutes: number) {
  try {
    localStorage.setItem(KEY, String(minutes));
  } catch {
    /* the default (1 minute) stays */
  }
}
