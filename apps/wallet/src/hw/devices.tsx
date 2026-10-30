// Hardware wallets we know, with a tiny drawing of each so they're easy to
// tell apart at a glance.

export type Connect = "trezor" | "bitbox" | "jade" | "ledger" | "qr";

export interface DeviceDef {
  id: string;
  /** Stored on the account as its device name. */
  name: string;
  connect: Connect;
  /** How it connects, for the device picker (phone). */
  via: string;
  /** How it connects on a computer, when that differs. */
  viaDesktop?: string;
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
    id: "ledger-flex",
    name: "Ledger Flex",
    connect: "ledger",
    via: "Plugged in by USB, or Bluetooth",
    viaDesktop: "USB cable or Bluetooth",
    exportHint: "Unlock your Ledger. The Bitcoin app opens when the wallet asks.",
  },
  {
    id: "ledger-nano-gen5",
    name: "Ledger Nano Gen5",
    connect: "ledger",
    via: "Plugged in by USB, or Bluetooth",
    viaDesktop: "USB cable or Bluetooth",
    exportHint: "Unlock your Ledger. The Bitcoin app opens when the wallet asks.",
  },
  {
    id: "jade",
    name: "Jade",
    connect: "jade",
    via: "Bluetooth or QR code",
    viaDesktop: "USB cable, Bluetooth or QR code",
    exportHint: "On Jade: unlock, then Options › Wallet › Export Xpub. Scan the QR.",
  },
  {
    id: "passport-prime",
    name: "Passport Prime",
    connect: "qr",
    via: "Air-gapped, by QR code",
    viaDesktop: "Air-gapped, by QR code (webcam)",
    exportHint: "On Prime: open Bitcoin Wallet, tap your account, then Connect Wallet › Sparrow › Single-sig. Scan the animated QR.",
  },
  {
    id: "seedsigner",
    name: "SeedSigner",
    connect: "qr",
    via: "Air-gapped, by QR code",
    viaDesktop: "Air-gapped, by QR code (webcam)",
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

const onPhone = () => /android|iphone|ipad/i.test(navigator.userAgent);

/** How this device connects here (phone or computer). */
export function viaFor(d: DeviceDef): string {
  return !onPhone() && d.viaDesktop ? d.viaDesktop : d.via;
}

/** Jade, live: Bluetooth on a phone; USB cable or Bluetooth on a computer. */
export const jadeLiveLabel = () => (onPhone() ? "Bluetooth" : "USB / Bluetooth");
export const jadeLiveHint = () =>
  onPhone() ? "Turn on Jade and choose Bluetooth." : "Plug Jade in with a USB cable, or turn it on and choose Bluetooth.";

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
      // Portrait aluminium body, edge-to-edge touchscreen showing the
      // Trezor mark (a padlock: shackle over a shield).
      return (
        <>
          <rect x="5.5" y="1.5" width="13" height="21" rx="3.2" />
          <rect x="7" y="3" width="10" height="18" rx="2" {...screen} />
          <path d="M10.3 10.6V9.4a1.7 1.7 0 0 1 3.4 0v1.2" strokeWidth="1.2" />
          <path d="M9.6 10.6h4.8v3.1L12 15.2l-2.4-1.5z" strokeWidth="1.2" />
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
      // Landscape body with the screen in the middle, a jog wheel on the
      // top edge and a button at each side.
      return (
        <>
          <rect x="2" y="6" width="20" height="13" rx="3.5" />
          <rect x="6" y="8.5" width="12" height="8" rx="1" {...screen} />
          <path d="M15.5 6V4.6h3V6" />
          <path d="M2 11.2v2.6M22 11.2v2.6" strokeWidth="2.4" />
        </>
      );
    case "ledger-flex":
      // Portrait card with an edge-to-edge E Ink touchscreen.
      return (
        <>
          <rect x="4.5" y="2" width="15" height="20" rx="2.6" />
          <rect x="6.3" y="3.8" width="11.4" height="16.4" rx="1.2" {...screen} />
        </>
      );
    case "ledger-nano-gen5":
      // Smaller, rounder body, touchscreen, a key ring loop at the top.
      return (
        <>
          <rect x="6" y="4" width="12" height="17" rx="3.2" />
          <rect x="7.8" y="6" width="8.4" height="10.5" rx="1" {...screen} />
          <path d="M10.5 4V2.6h3V4" />
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
