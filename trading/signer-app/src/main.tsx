import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
// Order matters: tailwind first so app.css (unlayered) wins ties.
import "./styles/fonts.css";
import "./styles/tailwind.css";
import "./styles/app.css";
import "./styles/slice2.css";
import { initMode } from "./styles/mode";

// Apply the persisted Solana/Perpetuals accent mode before first paint.
initMode();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary root label="app">
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
