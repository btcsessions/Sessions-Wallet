import { useState } from "react";
import { LOCK_CHOICES, isComputer, lockAfterMinutes, setLockAfterMinutes } from "../state/autolock";
import { SelectRow } from "./ui";

/** Settings (computers): how long the app stays unlocked in the background. */
export function AutoLockSetting() {
  const [minutes, setMinutes] = useState(lockAfterMinutes());
  if (!isComputer) return null;
  return (
    <>
      <p className="group-label">Lock</p>
      <div className="group">
        <SelectRow
          label="Lock after"
          value={String(minutes)}
          options={LOCK_CHOICES.map((c) => ({ value: String(c.minutes), label: c.label }))}
          onChange={(v) => {
            setMinutes(Number(v));
            setLockAfterMinutes(Number(v));
          }}
        />
      </div>
      <p className="group-note">How long this computer stays unlocked while the wallet is in the background or its window is closed. You'll always need your passcode when the app starts.</p>
    </>
  );
}
