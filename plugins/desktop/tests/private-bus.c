#define _GNU_SOURCE
#include <systemd/sd-bus.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* A synthetic peer for the real libsystemd ABI. This executable only connects
 * to the explicit private Unix address passed by the fixture. */
static const char *log_path;
static volatile sig_atomic_t stopping;
static int held_read = -1;
static void stop(int sig) { (void)sig; stopping = 1; }
static void record(const char *kind) {
    FILE *out = fopen(log_path, "a");
    if (!out) _exit(70);
    fprintf(out, "{\"kind\":\"%s\"}\n", kind);
    if (fclose(out) != 0) _exit(70);
}
static int inhibit(sd_bus_message *m, void *userdata, sd_bus_error *error) {
    (void)userdata; (void)error;
    const char *what, *who, *why, *mode;
    int rc = sd_bus_message_read(m, "ssss", &what, &who, &why, &mode);
    if (rc < 0) return rc;
    if (strcmp(what, "idle:sleep") || strcmp(who, "zcalld") ||
        strcmp(why, "An agent request is active or recently completed") || strcmp(mode, "block") || held_read >= 0)
        return sd_bus_reply_method_errorf(m, SD_BUS_ERROR_INVALID_ARGS, "invalid synthetic inhibit contract");
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_CLOEXEC | O_NONBLOCK) != 0) return -errno;
    held_read = pipe_fds[0];
    rc = sd_bus_reply_method_return(m, "h", pipe_fds[1]);
    close(pipe_fds[1]);
    if (rc < 0) { close(held_read); held_read = -1; return rc; }
    record("inhibit");
    return rc;
}
static int notify(sd_bus_message *m, void *userdata, sd_bus_error *error) {
    (void)userdata; (void)error;
    const char *app, *icon, *summary, *body, *action;
    uint32_t replaces;
    int32_t timeout;
    int rc = sd_bus_message_read(m, "susss", &app, &replaces, &icon, &summary, &body);
    if (rc < 0) return rc;
    bool valid = !strcmp(app, "zcalld") && replaces == 0 && !strcmp(icon, "") &&
        !strcmp(summary, "Usage warning") && strstr(body, "95.0%") != NULL;
    rc = sd_bus_message_enter_container(m, 'a', "s");
    if (rc < 0) return rc;
    rc = sd_bus_message_read(m, "s", &action);
    if (rc < 0) return rc;
    valid = valid && rc == 0;
    if ((rc = sd_bus_message_exit_container(m)) < 0) return rc;
    if ((rc = sd_bus_message_enter_container(m, 'a', "{sv}")) < 0) return rc;
    valid = valid && sd_bus_message_at_end(m, 0) > 0;
    if ((rc = sd_bus_message_exit_container(m)) < 0) return rc;
    if ((rc = sd_bus_message_read(m, "i", &timeout)) < 0) return rc;
    valid = valid && timeout == 10000 && sd_bus_message_at_end(m, 1) > 0;
    if (!valid) return sd_bus_reply_method_errorf(m, SD_BUS_ERROR_INVALID_ARGS, "invalid synthetic notification contract");
    record("notify");
    return sd_bus_reply_method_return(m, "u", 1u);
}
static const sd_bus_vtable login_vtable[] = {
    SD_BUS_VTABLE_START(0),
    SD_BUS_METHOD("Inhibit", "ssss", "h", inhibit, SD_BUS_VTABLE_UNPRIVILEGED),
    SD_BUS_VTABLE_END
};
static const sd_bus_vtable notification_vtable[] = {
    SD_BUS_VTABLE_START(0),
    SD_BUS_METHOD("Notify", "susssasa{sv}i", "u", notify, SD_BUS_VTABLE_UNPRIVILEGED),
    SD_BUS_VTABLE_END
};
int main(int argc, char **argv) {
    if (argc != 3 || strncmp(argv[1], "unix:path=", 10) || strchr(argv[1], ';')) return 64;
    log_path = argv[2];
    signal(SIGTERM, stop); signal(SIGINT, stop);
    sd_bus *bus = NULL;
    sd_bus_slot *login_slot = NULL, *notification_slot = NULL;
    int rc = sd_bus_new(&bus);
    if (rc < 0) goto out;
    if ((rc = sd_bus_set_address(bus, argv[1])) < 0 || (rc = sd_bus_set_bus_client(bus, 1)) < 0 ||
        (rc = sd_bus_start(bus)) < 0) goto out;
    if ((rc = sd_bus_add_object_vtable(bus, &login_slot, "/org/freedesktop/login1", "org.freedesktop.login1.Manager", login_vtable, NULL)) < 0 ||
        (rc = sd_bus_add_object_vtable(bus, &notification_slot, "/org/freedesktop/Notifications", "org.freedesktop.Notifications", notification_vtable, NULL)) < 0 ||
        (rc = sd_bus_request_name(bus, "org.freedesktop.login1", 0)) < 0 ||
        (rc = sd_bus_request_name(bus, "org.freedesktop.Notifications", 0)) < 0) goto out;
    record("ready");
    while (!stopping) {
        if (held_read >= 0) {
            char ignored;
            ssize_t n = read(held_read, &ignored, 1);
            if (n == 0) { close(held_read); held_read = -1; record("released"); }
            else if (n < 0 && errno != EAGAIN && errno != EINTR) { rc = -errno; goto out; }
        }
        rc = sd_bus_process(bus, NULL);
        if (rc < 0) goto out;
        if (rc == 0) {
            rc = sd_bus_wait(bus, 25000);
            if (rc < 0 && rc != -EINTR) goto out;
        }
    }
    rc = 0;
out:
    if (held_read >= 0) close(held_read);
    sd_bus_slot_unref(login_slot); sd_bus_slot_unref(notification_slot);
    sd_bus_flush_close_unref(bus);
    return rc < 0 ? 70 : 0;
}
