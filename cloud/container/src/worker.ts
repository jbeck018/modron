// modron cloud container worker: orchestrates modron serve instances
// running in Cloudflare Containers. Phase 2 scaffold - requires CF
// Containers (beta) to deploy. See Dockerfile for the container image.
export interface Env {}

export default {
  async fetch(request: Request): Promise<Response> {
    return new Response(
      "modron cloud container runtime - phase 2 scaffold (see Dockerfile + entrypoint.sh)"
    );
  },
};
