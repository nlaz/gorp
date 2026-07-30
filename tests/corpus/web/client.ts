// HTTP client for the jobs API: timeouts, retries, and typed responses.

export interface RequestOptions {
  timeoutMs: number;
  maxRetries: number;
  headers: Record<string, string>;
}

export const DEFAULT_OPTIONS: RequestOptions = {
  timeoutMs: 10_000,
  maxRetries: 3,
  headers: { "content-type": "application/json" },
};

export class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly body: string,
  ) {
    super(`http ${status}`);
  }
}

/** Abort a request once the deadline passes so a hung socket cannot pin a
 * worker slot forever. */
function withDeadline(timeoutMs: number): AbortSignal {
  const controller = new AbortController();
  setTimeout(() => controller.abort(), timeoutMs);
  return controller.signal;
}

function isRetryable(status: number): boolean {
  return status === 429 || status >= 500;
}

export class JobsClient {
  constructor(
    private readonly baseUrl: string,
    private readonly options: RequestOptions = DEFAULT_OPTIONS,
  ) {}

  async submitJob(kind: string, payload: unknown): Promise<string> {
    const response = await this.request("POST", "/jobs", { kind, payload });
    const parsed = (await response.json()) as { id: string };
    return parsed.id;
  }

  async fetchJobStatus(id: string): Promise<string> {
    const response = await this.request("GET", `/jobs/${encodeURIComponent(id)}`);
    const parsed = (await response.json()) as { status: string };
    return parsed.status;
  }

  /** Single request with bounded retries on transient failures. */
  private async request(method: string, path: string, body?: unknown): Promise<Response> {
    let lastError: unknown;
    for (let attempt = 0; attempt <= this.options.maxRetries; attempt++) {
      try {
        const response = await fetch(`${this.baseUrl}${path}`, {
          method,
          headers: this.options.headers,
          body: body === undefined ? undefined : JSON.stringify(body),
          signal: withDeadline(this.options.timeoutMs),
        });
        if (!response.ok) {
          if (isRetryable(response.status) && attempt < this.options.maxRetries) {
            await sleep(2 ** attempt * 100);
            continue;
          }
          throw new HttpError(response.status, await response.text());
        }
        return response;
      } catch (err) {
        lastError = err;
        if (attempt === this.options.maxRetries) break;
        await sleep(2 ** attempt * 100);
      }
    }
    throw lastError;
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
