// Hardware wallets we know, with a tiny drawing of each so they're easy to
// tell apart at a glance.

export type Connect = "trezor" | "bitbox" | "jade" | "qr";

export interface DeviceDef {
  id: string;
  /** Stored on the account as its device name. */
  name: string;
  connect: Connect;
  /** How it connects, for the device picker. */
  via: string;
  /** One-line how-to for exporting the wallet by QR. */
  exportHint: string;
}

/** Devices we support and have tested. More get added as they're tested. */
export const DEVICES: DeviceDef[] = [
  {
    id: "trezor-safe-7",
    name: "Trezor Safe 7",
    connect: "trezor",
    via: "USB or Bluetooth, via Trezor Suite",
    exportHint: "Connects through the Trezor Suite app.",
  },
  {
    id: "bitbox02",
    name: "BitBox02",
    connect: "bitbox",
    via: "Plugged in by USB",
    exportHint: "Plug in your BitBox02 and unlock it with your password.",
  },
  {
    id: "jade",
    name: "Jade",
    connect: "jade",
    via: "Bluetooth or QR code",
    exportHint: "On Jade: unlock, then Options › Wallet › Export Xpub. Scan the QR.",
  },
  {
    id: "passport-prime",
    name: "Passport Prime",
    connect: "qr",
    via: "Air-gapped, by QR code",
    exportHint: "On Prime: open Bitcoin Wallet, tap your account, then Connect Wallet › Sparrow › Single-sig. Scan the animated QR.",
  },
  {
    id: "seedsigner",
    name: "SeedSigner",
    connect: "qr",
    via: "Air-gapped, by QR code",
    exportHint: "On SeedSigner: load your seed, then Export Xpub › Single Sig › Native SegWit › Sparrow. Scan the animated QR.",
  },
];

/** Accounts from a device that isn't in the list (older imports). */
const GENERIC: DeviceDef = {
  id: "other",
  name: "Hardware wallet",
  connect: "qr",
  via: "By QR code",
  exportHint: "",
};

/** The device definition for a stored device name (older accounts used short names). */
export function deviceFor(name: string | undefined): DeviceDef {
  const n = (name ?? "").toLowerCase().replace(/\s+/g, "");
  return DEVICES.find((d) => d.name.toLowerCase().replace(/\s+/g, "") === n) ?? DEVICES.find((d) => d.id !== "other" && n.includes(d.id.split("-")[0])) ?? GENERIC;
}

/** Line drawings on a 24px grid, in the same stroke style as the app icons. */
function Drawing({ id }: { id: string }) {
  const screen = { fill: "currentColor", fillOpacity: 0.18 };
  switch (id) {
    case "trezor-safe-7":
      return (
        <>
          <rect x="6.5" y="1.5" width="11" height="21" rx="3" />
          <rect x="8.5" y="4" width="7" height="12.5" rx="1" {...screen} />
          <path d="M10.6 10.2V9.3a1.4 1.4 0 0 1 2.8 0v.9M10 10.2h4v2.6h-4z" strokeWidth="1.2" />
          <path d="M11 19.5h2" />
        </>
      );
    case "seedsigner":
      return (
        <>
          <rect x="1.5" y="6" width="21" height="12" rx="2" />
          <rect x="8.5" y="8" width="7.5" height="8" rx="0.6" {...screen} />
          <circle cx="4.9" cy="12" r="1.7" />
          <circle cx="19.4" cy="9" r="0.6" fill="currentColor" />
          <circle cx="19.4" cy="12" r="0.6" fill="currentColor" />
          <circle cx="19.4" cy="15" r="0.6" fill="currentColor" />
        </>
      );
    case "passport-prime":
      return (
        <>
          <rect x="5" y="1.5" width="14" height="21" rx="3.5" />
          <rect x="7" y="4" width="10" height="12" rx="1.2" {...screen} />
          <path d="M10 19.2h4" />
        </>
      );
    case "jade":
      return (
        <>
          <rect x="3.5" y="3" width="17" height="18" rx="5" />
          <rect x="7" y="6" width="10" height="7.5" rx="1" {...screen} />
          <circle cx="12" cy="17.2" r="1.6" />
        </>
      );
    case "bitbox02":
      return (
        <>
          <rect x="2" y="8.5" width="16" height="7" rx="1.8" />
          <path d="M18 10.3h3.5v3.4H18" />
          <rect x="4.5" y="10.5" width="7" height="3" rx="0.4" {...screen} />
        </>
      );
    default:
      return (
        <>
          <rect x="7" y="2" width="10" height="20" rx="2.5" />
          <rect x="9" y="5" width="6" height="6" rx="0.6" {...screen} />
          <path d="M11 18h2" />
        </>
      );
  }
}

export function DeviceIcon({ device, size = 20 }: { device: string | undefined; size?: number }) {
  const d = deviceFor(device);
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.6}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
      className="device-icon"
    >
      <Drawing id={d.id} />
    </svg>
  );
}

/** "Hardware · SeedSigner" pill, so cold storage never looks like a hot wallet. */
export function HardwareBadge({ device, compact }: { device: string; compact?: boolean }) {
  const def = deviceFor(device);
  const label = def.id !== "other" ? def.name : /hardware wallet|other device|cold storage/i.test(device) ? null : device;
  return (
    <span className="hw-badge" title={`Hardware wallet: ${device}`}>
      <DeviceIcon device={device} size={14} />
      <span>Hardware</span>
      {label && !compact && (
        <>
          <span className="hw-badge-sep" aria-hidden>
            ·
          </span>
          <span>{label}</span>
        </>
      )}
    </span>
  );
}
