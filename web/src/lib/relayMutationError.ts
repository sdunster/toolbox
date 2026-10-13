/**
 * Every admin mutation's root field is non-null (`Instance!`, `User!`, …), so
 * a resolver error (e.g. `ApiError::conflict("slug ... is already taken")`,
 * see `api/src/graphql/error.rs`) makes the whole response `data: null`.
 * `useMutation`'s `onError` then receives a `MutationFieldError` (or, for an
 * older code path, a plain `Error` whose `source.errors` was stashed by
 * `OperationExecutor._handleErrorResponse`) carrying the raw GraphQL
 * `errors` array either directly or under `.source` — this pulls the first
 * one's `message` back out either way, since that text (not just a code) is
 * meant to be shown to the admin, not only logged. Falls back to `err.message`
 * (which, empirically, already *is* the server's message for the common
 * case) and then to `fallback` for anything that carries neither.
 */
export function relayMutationErrorMessage(
  err: unknown,
  fallback: string,
): string {
  function firstMessage(container: unknown): string | undefined {
    if (
      !container ||
      typeof container !== "object" ||
      !("errors" in container)
    ) {
      return undefined;
    }
    const errors = (container as { errors?: unknown }).errors;
    if (!Array.isArray(errors) || errors.length === 0) return undefined;
    const message = (errors[0] as { message?: unknown } | undefined)?.message;
    return typeof message === "string" && message.length > 0
      ? message
      : undefined;
  }

  if (err && typeof err === "object") {
    const direct = firstMessage(err);
    if (direct) return direct;
    const nested = firstMessage((err as { source?: unknown }).source);
    if (nested) return nested;
    const message = (err as { message?: unknown }).message;
    if (typeof message === "string" && message.length > 0) return message;
  }
  return fallback;
}

/**
 * The first GraphQL error's `extensions.code` (`CONFLICT`, `NOT_FOUND`, …),
 * found the same two ways as {@link relayMutationErrorMessage} finds its
 * message, or `undefined`. For a form that shows the server's own text only
 * for an expected, user-actionable error and keeps its generic copy otherwise.
 */
export function relayMutationErrorCode(err: unknown): string | undefined {
  function firstCode(container: unknown): string | undefined {
    if (
      !container ||
      typeof container !== "object" ||
      !("errors" in container)
    ) {
      return undefined;
    }
    const errors = (container as { errors?: unknown }).errors;
    if (!Array.isArray(errors) || errors.length === 0) return undefined;
    const code = (errors[0] as { extensions?: { code?: unknown } } | undefined)
      ?.extensions?.code;
    return typeof code === "string" ? code : undefined;
  }

  if (err && typeof err === "object") {
    return firstCode(err) ?? firstCode((err as { source?: unknown }).source);
  }
  return undefined;
}
