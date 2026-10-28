// A small set of SF Symbols–style line icons, drawn on a 24px grid.

const paths: Record<string, string> = {
  up: "M7 17 17 7M8 7h9v9",
  down: "M17 7 7 17M16 17H7V8",
  swap: "M4 8h14l-4-4M20 16H6l4 4",
  bolt: "M13 2 4 14h7l-1 8 9-12h-7l1-8Z",
  // A safe's combination dial: numbered ticks over the knob.
  vault: "M2.5 12a9.5 9.5 0 1 0 19 0 9.5 9.5 0 1 0-19 0M9 13.5a3 3 0 1 0 6 0 3 3 0 1 0-6 0M5.40 12.00L3.60 12.00M6.28 8.70L4.73 7.80M8.70 6.28L7.80 4.73M12.00 5.40L12.00 3.60M15.30 6.28L16.20 4.73M17.72 8.70L19.27 7.80M18.60 12.00L20.40 12.00",
  // A shield with the vault's people inside: a group vault.
  groupVault: "M12 2.5 20 5.5v6c0 5-3.5 8.5-8 10-4.5-1.5-8-5-8-10v-6zM11 9.5a1 1 0 1 0 2 0 1 1 0 1 0-2 0M10 13.5a2 2 0 0 1 4 0M6.8 11a1 1 0 1 0 2 0 1 1 0 1 0-2 0M15.2 11a1 1 0 1 0 2 0 1 1 0 1 0-2 0M5.9 15a1.9 1.9 0 0 1 3.4-1M18.1 15a1.9 1.9 0 0 0-3.4-1",
  key: "M15 7a4 4 0 1 1-3.9 4.9L4 19v2h3v-2h2v-2h2l1.3-1.3A4 4 0 0 1 15 7Zm1 0h.01",
  device: "M8 2h8v20H8zM11 18h2M10 6h4v5h-4z",
  phone: "M8 2h8a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2ZM11 18.5h2",
  // A laptop.
  desktop: "M5 5h14a1 1 0 0 1 1 1v9H4V6a1 1 0 0 1 1-1ZM2 18h20",
  drop: "M12 3s6 6.6 6 11a6 6 0 0 1-12 0c0-4.4 6-11 6-11Z",
  coins: "M8 6c0-1.7 3.1-3 7-3s7 1.3 7 3-3.1 3-7 3-7-1.3-7-3Zm0 0v4c0 1.7 3.1 3 7 3s7-1.3 7-3V6M2 12c0-1.7 3.1-3 7-3M2 12v4c0 1.7 3.1 3 7 3s7-1.3 7-3v-1M2 12c0 1.7 3.1 3 7 3",
  list: "M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01",
  chart: "M3 17l5-5 4 4 8-9M15 7h5v5",
  gauge: "M4 18a9 9 0 1 1 16 0M12 13l4-5M12 14a1 1 0 1 0 0-2 1 1 0 0 0 0 2Z",
  eye: "M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12Zm10 3a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z",
  plus: "M12 5v14M5 12h14",
  minus: "M5 12h14",
  gear: "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6ZM19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z",
  check: "M5 12.5 10 17 19 7",
  close: "M6 6l12 12M18 6 6 18",
  copy: "M9 9h11v11H9zM5 15H4V4h11v1",
  scan: "M3 8V5a2 2 0 0 1 2-2h3M16 3h3a2 2 0 0 1 2 2v3M21 16v3a2 2 0 0 1-2 2h-3M8 21H5a2 2 0 0 1-2-2v-3M7 12h10",
  link: "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1",
  stack: "M12 3 3 8l9 5 9-5-9-5ZM3 12.5l9 5 9-5M3 17l9 5 9-5",
  info: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM12 11v5M12 8h.01",
  clock: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM12 7v5l3.2 2",
  photo: "M4 6h16v13H4zM4 16l5-5 4 4 2-2 5 5M15.5 9.5h.01",
  paste: "M9 4h6v3H9zM15 5h3v16H6V5h3",
  warn: "M12 3 2 20h20L12 3Zm0 6v5m0 3h.01",
  lock: "M6 11h12v10H6zM8 11V8a4 4 0 1 1 8 0v3",
  resize: "M14 4h6v6M10 20H4v-6M20 4l-7 7M4 20l7-7",
  people: "M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8Zm-7 10v-1a6 6 0 0 1 12 0v1M16 3.1a4 4 0 0 1 0 7.8M22 21v-1a6 6 0 0 0-4-5.7",
  shield: "M12 3 4 6v6c0 5 3.4 8.4 8 9 4.6-.6 8-4 8-9V6l-8-3Zm-3 9 2 2 4-4",
  server: "M4 4h16v6H4zM4 14h16v6H4zM8 7h.01M8 17h.01",
  ticket: "M4 7a2 2 0 0 0 2-2h12a2 2 0 0 0 2 2v3a2 2 0 0 0 0 4v3a2 2 0 0 0-2 2H6a2 2 0 0 0-2-2v-3a2 2 0 0 0 0-4V7Z",
  chevron: "M9 6l6 6-6 6",
  user: "M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8Zm-7 9v-1a6 6 0 0 1 6-6h2a6 6 0 0 1 6 6v1",
  edit: "M4 20h4L19 9l-4-4L4 16v4ZM14 6l4 4",
  trash: "M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3",
  bell: "M6 16V11a6 6 0 1 1 12 0v5l2 2H4l2-2ZM10 21h4",
  plug: "M9 3v5M15 3v5M7 8h10v4a5 5 0 0 1-10 0V8ZM12 17v4",
  back: "M15 6l-6 6 6 6",
  refresh: "M20 11a8 8 0 0 0-14.9-3M4 4v4h4M4 13a8 8 0 0 0 14.9 3M20 20v-4h-4",
};

export type IconName = keyof typeof paths;

export function Icon({ name, size = 20, stroke = 2, title }: { name: IconName; size?: number; stroke?: number; title?: string }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={stroke}
      strokeLinecap="round"
      strokeLinejoin="round"
      role={title ? "img" : undefined}
      aria-hidden={title ? undefined : true}
      aria-label={title}
    >
      <path d={paths[name]} />
    </svg>
  );
}
