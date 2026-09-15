import type { MetadataRoute } from "next";

/** Next.js app-route manifest (also mirrored in public/manifest.json for SW). */
export default function manifest(): MetadataRoute.Manifest {
  return {
    name: "Opinions",
    short_name: "Opinions",
    description: "Live opinion markets — vote, then trade.",
    start_url: "/",
    display: "standalone",
    background_color: "#070a0e",
    theme_color: "#070a0e",
    orientation: "portrait-primary",
    icons: [
      {
        src: "/icons/icon-192.png",
        sizes: "192x192",
        type: "image/png",
      },
      {
        src: "/icons/icon-512.png",
        sizes: "512x512",
        type: "image/png",
      },
    ],
  };
}
