#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Meant to be executed by background isolates needing access to the account manager (e.g., FCM)
 */
char *background_processor(const char *packet, const char *home_dir);

int32_t error_message_utf8(char *buf, int32_t length);

int32_t last_error_length(void);

int32_t load_page(int64_t port, const char *home_dir);

int32_t memfree(const char *ptr);

char *send_to_kernel(const char *packet);

#ifdef __cplusplus
} // extern "C"
#endif // __cplusplus