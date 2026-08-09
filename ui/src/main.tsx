import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { createQueryClient } from "./lib/queryClient";
import "./styles.css";

const queryClient = createQueryClient();

const rootEl = document.getElementById("root");
if (!rootEl) {
  throw new Error("darkmux: #root element missing from index.html");
}

createRoot(rootEl).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </StrictMode>,
);
