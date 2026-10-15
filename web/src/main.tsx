import "./lib/zod-config";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createQueryClient } from "./lib/query-client";
import { createAppRouter } from "./router";
import "./styles.css";

const queryClient = createQueryClient();
const router = createAppRouter(undefined, queryClient);

const root = document.getElementById("root");
if (!root) throw new Error("missing #root element");

createRoot(root).render(
	<StrictMode>
		<RouterProvider router={router} />
	</StrictMode>,
);
