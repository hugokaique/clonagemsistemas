import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";

async function boot() {
  // `npm run dev` aberto num navegador comum (fora do Tauri): instala um
  // backend simulado para desenvolver a UI sem elevação nem discos reais.
  // O bloco inteiro é eliminado do build de produção.
  if (import.meta.env.DEV && !("__TAURI_INTERNALS__" in window)) {
    const { installDevMock } = await import("./devMock");
    installDevMock();
  }
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>
  );
}

boot();
