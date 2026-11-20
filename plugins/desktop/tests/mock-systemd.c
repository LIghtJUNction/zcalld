/* Only loaded by isolated Linux fixtures via a private LD_LIBRARY_PATH. */
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct item { int fd; };
static void note(const char *event) {
    const char *path = getenv("ZCALLD_DESKTOP_MOCK_LOG");
    if (!path) return;
    FILE *f = fopen(path, "a"); if (!f) return;
    fprintf(f, "{\"kind\":\"%s\"}\n", event); fclose(f);
}
int sd_bus_new(void **out) {
    struct item *p = calloc(1, sizeof(*p)); if (!p) return -ENOMEM;
    p->fd = -1; *out = p; note("new"); return 0;
}
void *sd_bus_unref(void *value) { free(value); note("close"); return NULL; }
void *sd_bus_message_unref(void *value) {
    if (value) { struct item *p = value; if (p->fd >= 0) close(p->fd); free(p); }
    return NULL;
}
int sd_bus_set_address(void *bus, const char *address) {
    (void)bus; if (strncmp(address, "unix:", 5) != 0) return -EINVAL;
    note("local_address"); return 0;
}
int sd_bus_set_bus_client(void *bus, int flag) { (void)bus; return flag == 1 ? 0 : -EINVAL; }
int sd_bus_set_method_call_timeout(void *bus, uint64_t timeout) {
    (void)bus; if (timeout != 500000) return -EINVAL; note("bounded_timeout"); return 0;
}
int sd_bus_start(void *bus) { (void)bus; return 0; }
int sd_bus_call_method(void *bus, const char *dest, const char *path, const char *interface,
                       const char *member, void *error, void **reply, const char *types, ...) {
    (void)bus; (void)dest; (void)path; (void)interface; (void)error;
    if (getenv("ZCALLD_DESKTOP_MOCK_DENY")) return -EACCES;
    struct item *p = calloc(1, sizeof(*p)); if (!p) return -ENOMEM; p->fd = -1;
    if (strcmp(member, "Inhibit") == 0) {
        if (strcmp(types, "ssss") != 0) { free(p); return -EINVAL; }
        va_list args; va_start(args, types); const char *what = va_arg(args, const char *); va_end(args);
        if (strcmp(what, "idle:sleep") != 0) { free(p); return -EINVAL; }
        const char *hold = getenv("ZCALLD_DESKTOP_MOCK_HOLD");
        if (!hold || (p->fd = open(hold, O_RDONLY | O_CLOEXEC)) < 0) { free(p); return -EINVAL; }
        note("inhibit");
    } else if (strcmp(member, "Notify") == 0) {
        if (strcmp(types, "susssasa{sv}i") != 0) { free(p); return -EINVAL; }
        va_list args; va_start(args, types);
        const char *app = va_arg(args, const char *); unsigned replace = va_arg(args, unsigned);
        const char *icon = va_arg(args, const char *), *title = va_arg(args, const char *), *body = va_arg(args, const char *);
        unsigned actions = va_arg(args, unsigned), hints = va_arg(args, unsigned); int timeout = va_arg(args, int);
        va_end(args);
        if (strcmp(app, "zcalld") || replace || icon[0] || !title[0] || !body[0] || actions || hints || timeout != 10000) { free(p); return -EINVAL; }
        note("notify");
    } else { free(p); return -EINVAL; }
    *reply = p; return 1;
}
int sd_bus_message_read(void *reply, const char *types, ...) {
    if (strcmp(types, "h") != 0) return -EINVAL;
    va_list args; va_start(args, types); int *fd = va_arg(args, int *); va_end(args);
    *fd = ((struct item *)reply)->fd; return 1;
}
