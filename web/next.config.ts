import type { NextConfig } from "next";
import path from "node:path";

const coreProxyTarget = process.env.CORE_PROXY_TARGET?.replace(/\/$/, "");

const nextConfig: NextConfig = {
  // Keep file tracing inside web/ even if a parent lockfile exists.
  outputFileTracingRoot: path.join(__dirname),
  async rewrites() {
    if (!coreProxyTarget) return [];
    return [
      {
        source: "/core-api/:path*",
        destination: `${coreProxyTarget}/:path*`,
      },
    ];
  },
};

export default nextConfig;
