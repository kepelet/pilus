// The C ABI crates/pilus-mobile exports. Keep in sync with src/ffi.rs.
//
// String returns are heap-allocated UTF-8 owned by the caller; free them with
// pilus_string_free. PilusClient is an opaque handle freed by
// pilus_client_free. A NULL return means pilus_last_error has the message.

#ifndef pilus_mobile_h
#define pilus_mobile_h

typedef struct PilusClient PilusClient;

/* `{"endpoint_id": "<id>"}` for the device key under data_dir. */
char *pilus_device_id(const char *data_dir);

/* Bind a client to the host `pilus-agent serve --iroh` printed as `ticket`.
   `relay`/`api_secret` select a self-hosted relay; NULL/"" = n0 defaults. */
PilusClient *pilus_client_connect(const char *data_dir, const char *ticket,
                                  const char *relay, const char *api_secret);

/* Rebind from the saved enrollment under data_dir; NULL when none exists. */
PilusClient *pilus_client_resume(const char *data_dir,
                                 const char *relay, const char *api_secret);

/* Enroll with the host's printed code, then hold the delegated token. */
int pilus_client_enroll(PilusClient *client, const char *code, const char *label);

/* Refresh the token for an already-enrolled device. */
int pilus_client_authorize(PilusClient *client);

/* One UHP request; returns the reply `result` as a JSON string. */
char *pilus_client_request(PilusClient *client, const char *method, const char *params_json);

/* One hub request on `luvus/hub/1`; `machine` NULL/"" targets the paired host. */
char *pilus_client_hub_request(PilusClient *client, const char *machine, const char *method, const char *params_json);

/* Open `hub.watch`; returns the first reply. Then call pilus_hub_watch_next
   per signal until NULL, then reopen. */
char *pilus_hub_watch_open(PilusClient *client);
char *pilus_hub_watch_next(PilusClient *client);
void pilus_hub_watch_close(PilusClient *client);

/* The saved enrollment summary as JSON, or `null`. */
char *pilus_client_enrollment(const char *data_dir);

/* Delete the saved enrollment under data_dir. */
void pilus_forget(const char *data_dir);

void pilus_client_free(PilusClient *client);

/* A fresh copy of this thread's last error message, or NULL. */
char *pilus_last_error(void);

/* The last error as {"code","message","dispatch"} JSON, or NULL. `code` is the
   daemon's own (revision_conflict, agent_not_ready, ...) or transport, timeout,
   unconfirmed (a write that may have been applied), not_enrolled, ... */
char *pilus_last_error_detail(void);

void pilus_string_free(char *string);

#endif /* pilus_mobile_h */
