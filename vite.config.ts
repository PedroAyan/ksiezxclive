import { resolve } from "node:path";
import { defineConfig } from "vite";

export default defineConfig({
  build: {
    target: "chrome109",
    rollupOptions: {
      input: {
        desktop: resolve("index.html"),
        viewer: resolve("assistir.html"),
      },
    },
  },
});
