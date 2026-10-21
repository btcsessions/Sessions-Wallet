import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource-variable/inter";
import "./styles/app.css";
import { App } from "./App";
import { useWallet } from "./state/store";

// Last-resort error display: if anything throws before or outside React,
// show it instead of leaving a blank screen.
function showFatal(message: string) {
  const root = document.getElementById("root");
  // Only during startup; later errors are shown as toasts by the app itself.
  if (!root || root.dataset.fatal || useWallet.getState().status !== "loading") return;
  root.dataset.fatal = "1";
  root.innerHTML = "";
  const box = document.createElement("pre");
  box.style.cssText = "white-space:pre-wrap;padding:24px;font:13px ui-monospace,monospace;color:#ff453a;background:#000;min-height:100vh;margin:0";
  box.textContent = `Sessions Wallet hit an error while starting:\n\n${message}`;
  root.appendChild(box);
}
window.addEventListener("error", (e) => showFatal(`${e.message}\n${e.filename}:${e.lineno}`));
window.addEventListener("unhandledrejection", (e) => showFatal(String((e.reason as Error)?.stack ?? e.reason)));

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
