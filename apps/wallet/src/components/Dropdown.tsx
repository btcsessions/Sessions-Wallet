import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import type { AccountInfo } from "../api/types";
import { DeviceIcon } from "../hw/devices";
import { accountColor, accountKind, formatSats } from "../format";
import { Icon } from "./Icon";

export interface DropdownOption<T extends string> {
  value: T;
  label: string;
  /** Second line in the list. */
  detail?: string;
  /** Colour dot / bubble tint. */
  color?: string;
  icon?: ReactNode;
}

/**
 * An in-app dropdown: the current choice as a tinted bubble, and a list that
 * opens in place under it. Replaces the platform pop-up menu, which looks out
 * of place in the wallet.
 */
export function Dropdown<T extends string>({
  label,
  value,
  options,
  onChange,
  placeholder = "Choose",
}: {
  label: string;
  value: T | undefined;
  options: DropdownOption<T>[];
  onChange: (v: T) => void;
  placeholder?: string;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const listId = useId();
  const current = options.find((o) => o.value === value);

  // Close on a tap elsewhere or Escape (without closing the sheet).
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => ref.current && !ref.current.contains(e.target as Node) && setOpen(false);
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopImmediatePropagation();
      setOpen(false);
    };
    document.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open]);

  return (
    <div className={`dropdown${open ? " open" : ""}`} ref={ref}>
      <button type="button" className="cell dropdown-button" aria-haspopup="listbox" aria-expanded={open} aria-controls={listId} onClick={() => setOpen(!open)}>
        <span className="grow dropdown-label">{label}</span>
        <span className="bubble" style={current?.color ? { ["--bubble" as string]: current.color } : undefined}>
          {current?.icon ?? (current?.color && <span className="bubble-dot" />)}
          <span className="bubble-text">{current?.label ?? placeholder}</span>
        </span>
        <span className="dropdown-chevron" aria-hidden>
          <Icon name="chevron" size={14} stroke={2.5} />
        </span>
      </button>
      {open && (
        <ul className="dropdown-list" role="listbox" id={listId} aria-label={label}>
          {options.map((o) => (
            <li key={o.value}>
              <button
                type="button"
                role="option"
                aria-selected={o.value === value}
                className="dropdown-option"
                onClick={() => {
                  onChange(o.value);
                  setOpen(false);
                }}
              >
                <span className="option-mark" style={o.color ? { ["--bubble" as string]: o.color } : undefined}>
                  {o.icon ?? <span className="bubble-dot" />}
                </span>
                <span className="grow">
                  <span className="option-label">{o.label}</span>
                  {o.detail && <span className="option-detail">{o.detail}</span>}
                </span>
                {o.value === value && <Icon name="check" size={16} stroke={2.5} />}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Dropdown options for accounts: colour, kind and balance. */
export function accountOptions(accounts: AccountInfo[]): DropdownOption<string>[] {
  return accounts.map((a) => ({
    value: a.id,
    label: a.name,
    detail: `${accountKind(a)} · ${formatSats(a.balance.confirmed)} sats`,
    color: accountColor(a),
    icon: a.custody.type === "hardware" ? <DeviceIcon device={a.custody.device} size={14} /> : undefined,
  }));
}

export function AccountPicker({
  label,
  accounts,
  value,
  onChange,
}: {
  label: string;
  accounts: AccountInfo[];
  value: string | undefined;
  onChange: (id: string) => void;
}) {
  return <Dropdown label={label} value={value} options={accountOptions(accounts)} onChange={onChange} />;
}
