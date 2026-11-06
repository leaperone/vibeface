export function resolveBoundedPath(root: string, candidate: string): string;

export function ensurePrivateDirectory(
  directory: string,
  options?: { root?: string },
): Promise<string>;

export function verifyPrivateControlFile(
  filename: string,
  options?: { root?: string; maxBytes?: number },
): Promise<string>;

export function readPrivateControlJson(
  filename: string,
  options?: { root?: string; maxBytes?: number },
): Promise<unknown>;

export function writePrivateControlFile(
  filename: string,
  content: string,
  options?: { root?: string; maxBytes?: number },
): Promise<string>;
