#ifndef WETYPE_H
#define WETYPE_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * Opaque handle owning one `wetype-client` connection.
 */
typedef struct WetypeHandle WetypeHandle;

/**
 * One candidate. Pointers are owned by the enclosing `WetypeCandidates`.
 */
typedef struct WetypeCandidate {
  const uint8_t *text;
  uintptr_t text_len;
  uint32_t cover;
  bool cloud;
  const uint8_t *comment;
  uintptr_t comment_len;
} WetypeCandidate;

/**
 * Snapshot returned by key/backspace. Release with [`wetype_candidates_free`].
 *
 * All-zero is a valid empty snapshot, which is what `Default` produces and
 * what the free functions leave behind.
 */
typedef struct WetypeCandidates {
  uint64_t generation;
  struct WetypeCandidate *items;
  uintptr_t items_len;
  bool truncated;
  const uint8_t *preedit;
  uintptr_t preedit_len;
  uint32_t cursor;
  /**
   * Text the engine wants committed right now (punctuation, auto-commit).
   */
  const uint8_t *commit;
  uintptr_t commit_len;
  /**
   * Composition without display separators: preedit is "ni'hao", raw is
   * "nihao". This is what Enter should commit.
   */
  const uint8_t *raw;
  uintptr_t raw_len;
} WetypeCandidates;

/**
 * Result of a selection. Release with [`wetype_select_result_free`].
 */
typedef struct WetypeSelectResult {
  /**
   * Text to insert into the application now.
   */
  const uint8_t *commit;
  uintptr_t commit_len;
  /**
   * True when the composition ended; `rest` is empty then.
   */
  bool finished;
  /**
   * Composition that is still open after a partial commit. The engine host
   * has already removed the committed prefix, so this is an ordinary
   * snapshot of the remaining suffix.
   */
  struct WetypeCandidates rest;
} WetypeSelectResult;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Connect to the daemon socket. Returns NULL on failure.
 *
 * # Safety
 * `socket_path` must be a valid NUL-terminated string.
 */
struct WetypeHandle *wetype_connect(const char *socket_path);

/**
 * Release a handle. Null is a no-op.
 *
 * # Safety
 * `handle` must come from [`wetype_connect`] and be closed once.
 */
void wetype_close(struct WetypeHandle *handle);

/**
 * Last error message for this handle. Never NULL.
 *
 * The returned pointer is a thread-local copy, valid until the same thread
 * calls this function again.
 *
 * # Safety
 * `handle` must be a live handle, or NULL for errors raised before one exists.
 */
const char *wetype_last_error(const struct WetypeHandle *handle);

/**
 * Connection generation for this handle.
 *
 * Bumped on every successful (re)connect. A frontend that holds engine session
 * ids must compare this with the value it saw when it created them: a
 * different value means the daemon restarted, and every session it holds is
 * dead. The client reconnects on its own, so the caller only has to rebuild
 * its sessions.
 *
 * # Safety
 * `handle` must be a live handle, or NULL (which yields 0).
 */
uint64_t wetype_generation(const struct WetypeHandle *handle);

/**
 * Whether the daemon socket is connected right now.
 *
 * The client keeps reconnecting on its own, so this is a snapshot: `false`
 * means "the daemon is missing or restarting", not "give up". A frontend can
 * poll it to label its UI without waiting for a failed request.
 *
 * # Safety
 * `handle` must be a live handle, or NULL (which yields false).
 */
bool wetype_connected(const struct WetypeHandle *handle);

/**
 * Wire protocol version this build speaks.
 *
 * Exported as a function (cbindgen cannot render a constant that points at
 * another crate) so a host can refuse a mismatched daemon instead of silently
 * losing fields during its decode/re-encode.
 */
uint32_t wetype_protocol_version(void);

/**
 * Round-trip a ping. Returns 0 on success.
 *
 * # Safety
 * `handle` must be a live handle.
 */
int wetype_ping(struct WetypeHandle *handle, uint32_t protocol_version);

/**
 * # Safety
 * `handle` live, `app_name`/`smart`/`fuzzy` NULL or NUL-terminated,
 * `out_session` writable.
 */
int wetype_new_session(struct WetypeHandle *handle,
                       uint32_t ctx_id,
                       const char *app_name,
                       const char *smart,
                       const char *fuzzy,
                       uint64_t *out_session);

/**
 * # Safety
 * `handle` must be live.
 */
int wetype_close_session(struct WetypeHandle *handle, uint64_t session);

/**
 * # Safety
 * `handle` must be live.
 */
int wetype_reset(struct WetypeHandle *handle, uint64_t session);

/**
 * Send one printable ASCII key. Returns 0 and fills `out` on success.
 *
 * # Safety
 * `handle` live, `out` writable.
 */
int wetype_key(struct WetypeHandle *handle,
               uint64_t session,
               uint8_t key,
               struct WetypeCandidates *out);

/**
 * # Safety
 * `handle` live, `out` writable.
 */
int wetype_backspace(struct WetypeHandle *handle,
                     uint64_t session,
                     uint32_t count,
                     struct WetypeCandidates *out);

/**
 * Select by index into the snapshot identified by `generation`.
 *
 * # Safety
 * `handle` live, `out` writable.
 */
int wetype_select(struct WetypeHandle *handle,
                  uint64_t session,
                  uint64_t generation,
                  uint32_t index,
                  struct WetypeSelectResult *out);

/**
 * Release a candidates snapshot. Safe on a zeroed struct.
 *
 * # Safety
 * `c` must come from `wetype_key`/`wetype_backspace` and be freed once.
 */
void wetype_candidates_free(struct WetypeCandidates *c);

/**
 * Release a selection result. Safe on a zeroed struct.
 *
 * # Safety
 * `r` must come from `wetype_select` and be freed once.
 */
void wetype_select_result_free(struct WetypeSelectResult *r);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* WETYPE_H */
