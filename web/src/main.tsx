import { QueryClient } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createAppRouter } from "./router";
import "./styles.css";

const queryClient = new QueryClient();
const router = createAppRouter(undefined, queryClient);

const root = document.getElementById("root");
if (!root) throw new Error("missing #root element");

createRoot(root).render(
	<StrictMode>
		<RouterProvider router={router} />
	</StrictMode>,
);
