/** `code` values of the server's `{"error": {"code", "message"}}` envelope (see docs/api.md). */
export type ServerErrorCode =
  | "bad_request"
  | "malformed_payload"
  | "unauthorized"
  | "invalid_signature"
  | "forbidden"
  | "not_found"
  | "store_not_found"
  | "method_not_allowed"
  | "conflict"
  | "transfer_refused"
  | "payload_too_large"
  | "store_credentials"
  | "not_implemented"
  | "store_unavailable"
  | "internal";

export type EntitleErrorCode =
  | ServerErrorCode
  /** The request never got an HTTP response (offline, DNS, CORS, aborted). `status` is 0. */
  | "network_error"
  /** A response that is not the documented JSON (e.g. a proxy's HTML error page). */
  | "unexpected_response";

/** Every failed SDK call rejects with this. */
export class EntitleError extends Error {
  override readonly name = "EntitleError";

  constructor(
    /** HTTP status, or 0 for `network_error`. */
    readonly status: number,
    /** Server error code, `network_error` or `unexpected_response`. */
    readonly code: EntitleErrorCode | (string & {}),
    message: string,
    cause?: unknown,
  ) {
    super(message, cause === undefined ? undefined : { cause });
  }

  get isNetworkError(): boolean {
    return this.code === "network_error";
  }
}
