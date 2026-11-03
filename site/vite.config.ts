import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"

// relative asset paths: the built site works from any folder or host
export default defineConfig({ plugins: [react()], base: "./" })
