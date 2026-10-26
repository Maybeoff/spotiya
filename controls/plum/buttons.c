#define _POSIX_C_SOURCE 200809L
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include "config.h"

enum action { TOGGLE, NEXT, PREV, LOUDER, QUIETER };
enum { GESTURE_US = 250000, REPEAT_DELAY_US = 450000, REPEAT_US = 180000 };
struct key {
    int down, suppressed;
    int64_t repeat_at;
};
struct gesture {
    struct key keys[3];
    int armed, consumed;
    int64_t pending;
};
static enum action queue[16];
static size_t queued;

static void emit(enum action a) {
    if (queued < 16)
        queue[queued++] = a;
}
static int key_index(unsigned code) {
    return code == control_config.volume_down ? 0
           : code == control_config.volume_up ? 1
           : code == control_config.alice     ? 2
                                              : -1;
}
static void tick(struct gesture *g, int64_t now) {
    if (g->pending && now >= g->pending) {
        emit(TOGGLE);
        g->pending = 0;
        g->armed = 0;
    }
    for (int i = 0; i < 2; i++) {
        struct key *k = &g->keys[i];
        if (k->down && !k->suppressed && now >= k->repeat_at) {
            emit(i == 1 ? LOUDER : QUIETER);
            k->repeat_at = now + REPEAT_US;
        }
    }
}
static inline int64_t gesture_deadline(const struct gesture *g) {
    int64_t next = g->pending;
    for (int i = 0; i < 2; i++) {
        const struct key *k = &g->keys[i];
        if (k->down && !k->suppressed && (!next || k->repeat_at < next))
            next = k->repeat_at;
    }
    return next;
}
static void key_event(struct gesture *g, unsigned code, int value, int64_t now) {
    tick(g, now);
    int i = key_index(code);
    if (i < 0 || value == 2)
        return;
    struct key *k = &g->keys[i];
    if (value == 0) {
        if (!k->down)
            return;
        memset(k, 0, sizeof *k);
        if (i == 2 && g->armed && !g->consumed)
            g->pending = now + GESTURE_US;
        if (!g->keys[0].down && !g->keys[1].down && !g->keys[2].down && g->consumed)
            g->armed = g->consumed = 0;
        return;
    }
    if (value != 1 || k->down)
        return;
    k->down = 1;
    if (i == 2) {
        if (g->pending)
            emit(TOGGLE);
        g->pending = 0;
        g->armed = 1;
        g->consumed = 0;
        k->suppressed = 1;
    } else if (g->armed && (g->keys[2].down || g->pending)) {
        g->pending = 0;
        g->armed = 0;
        g->consumed = 1;
        k->suppressed = 1;
        emit(i == 1 ? NEXT : PREV);
    } else if (g->keys[2].down && g->consumed) {
        k->suppressed = 1;
    } else {
        k->repeat_at = now + REPEAT_DELAY_US;
        emit(i == 1 ? LOUDER : QUIETER);
    }
}

#ifndef BUTTON_TEST
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>
#include "lights.h"
#include "display.h"

#define EVIOCGRAB 0x40044590UL
#define EVIOCGKEY96 0x80604518UL
#define EVIOCSCLOCKID 0x400445a0UL
struct input_event_abi {
    long sec, usec;
    uint16_t type, code;
    int32_t value;
};
static volatile sig_atomic_t running = 1;
static int current_paused, current_volume, current_steps = 100;
static const char *action_names[] = {"playpause", "next", "prev", "volume+", "volume-"};
static void note(const char *s) {
    size_t left = strlen(s);
    while (left) {
        ssize_t written = write(STDERR_FILENO, s, left);
        if (written < 0 && errno == EINTR)
            continue;
        if (written <= 0)
            return;
        s += written;
        left -= (size_t)written;
    }
}
static int64_t now_us(clockid_t clock) {
    struct timespec t;
    if (clock_gettime(clock, &t))
        return 0;
    return (int64_t)t.tv_sec * 1000000 + t.tv_nsec / 1000;
}
extern int plum_receiver_status(int *paused, int *volume);
extern int plum_receiver_command(int action);
extern int plum_receiver_running(void);
extern int plum_receiver_spectrum(unsigned char *levels);
static int spotify_active(void) { return plum_receiver_status(&current_paused, &current_volume); }
static int command(enum action a) { return plum_receiver_command((int)a); }
static void earliest(int64_t *wake, int64_t at) {
    if (at && at < *wake)
        *wake = at;
}
static int empty_keys(const unsigned char *keys) {
    for (int i = 0; i < 96; i++)
        if (keys[i])
            return 0;
    return 1;
}
static void handle_signal(int sig) {
    (void)sig;
    running = 0;
}

int plum_buttons_run(const struct mini2_config *config) {
    control_config = *config;
    signal(SIGINT, handle_signal);
    signal(SIGTERM, handle_signal);
    signal(SIGHUP, handle_signal);
    int fd =
        control_config.input ? open(control_config.input, O_RDONLY | O_NONBLOCK | O_CLOEXEC) : -1;
    if (control_config.input && fd < 0) {
        note("buttons: cannot open input\n");
        return 1;
    }
    clockid_t event_clock = CLOCK_REALTIME;
    if (fd >= 0 && ioctl(fd, EVIOCSCLOCKID, &event_clock) < 0)
        event_clock = CLOCK_MONOTONIC;
    struct pollfd pfd = {fd, POLLIN, 0};
    struct input_event_abi events[64];
    unsigned char held[96] = {0}, actual[96] = {0};
    struct gesture g = {0};
    struct lights lights = {0};
    struct display display = {0};
    int grabbed = 0, wanted = 0;
    int64_t probe_at = now_us(CLOCK_MONOTONIC);
    if (fd >= 0)
        ioctl(fd, EVIOCGKEY96, held);
    note(fd >= 0 ? "buttons: ready (Alice control until Spotify is active)\n"
                  : "buttons: ready (no input device, Spotify control)\n");
    while (running) {
        int64_t now = now_us(CLOCK_MONOTONIC);
        if (now >= probe_at) {
            wanted = spotify_active();
            probe_at = now + 300000;
        }
        if (fd >= 0) {
            ssize_t n = read(fd, events, sizeof events);
            if (n > 0) {
                if (!grabbed)
                    ioctl(fd, EVIOCGKEY96, held);
                for (size_t i = 0; i < (size_t)n / sizeof events[0]; i++) {
                    struct input_event_abi *e = &events[i];
                    if (e->type == 0 && e->code == 3) {
                        if (grabbed)
                            ioctl(fd, EVIOCGRAB, 0);
                        grabbed = 0;
                        queued = 0;
                        memset(&g, 0, sizeof g);
                        if (ioctl(fd, EVIOCGKEY96, held) < 0)
                            memset(held, 0, sizeof held);
                    } else if (e->type == 1) {
                        if (e->code < 768) {
                            unsigned char mask = (unsigned char)(1u << (e->code % 8));
                            if (e->value)
                                held[e->code / 8] |= mask;
                            else
                                held[e->code / 8] &= (unsigned char)~mask;
                        }
                        if (grabbed && wanted)
                            key_event(&g, e->code, e->value, (int64_t)e->sec * 1000000 + e->usec);
                    }
                }
            }
            if (n < 0 && errno != EAGAIN && errno != EINTR) {
                note("buttons: input read failed\n");
                break;
            }
            if (wanted != grabbed && empty_keys(held)) {
                int released = !wanted || (ioctl(fd, EVIOCGKEY96, actual) >= 0 && empty_keys(actual));
                if (released) {
                    if (ioctl(fd, EVIOCGRAB, wanted) < 0) {
                        note("buttons: grab failed\n");
                        break;
                    }
                    grabbed = wanted;
                    queued = 0;
                    memset(&g, 0, sizeof g);
                    note(grabbed ? "buttons: Spotify control\n" : "buttons: Alice control\n");
                }
            }
        }
        if (grabbed && wanted)
            tick(&g, now_us(event_clock));
        for 
        for (size_t i = 0; i < queued && wanted && running; i++) {
            if (now_us(CLOCK_MONOTONIC) >= probe_at) {
                wanted = spotify_active();
                probe_at = now_us(CLOCK_MONOTONIC) + 300000;
                if (!wanted)
                    break;
            }
            note("button ");
            note(action_names[queue[i]]);
            int feedback_paused = queue[i] == TOGGLE ? !current_paused : current_paused;
            if (command(queue[i])) {
                note(" OK\n");
                wanted = spotify_active();
                int64_t now = now_us(CLOCK_MONOTONIC);
                probe_at = now + 300000;
                lights_begin(&lights, queue[i], feedback_paused, current_volume, current_steps,
                             now);
                if (queue[i] == LOUDER || queue[i] == QUIETER)
                    display_show(&display,
                                 current_steps > 0 ? current_volume * 100 / current_steps : 0, now);
                else
                    display_play(&display,
                                 queue[i] == NEXT   ? &control_config.clock_next
                                 : queue[i] == PREV ? &control_config.clock_previous
                                 : feedback_paused  ? &control_config.clock_pause
                                                    : NULL,
                                 now);
            } else {
                note(" failed; returning controls\n");
                wanted = 0;
                break;
            }
        }
        queued = 0;
        int64_t now = now_us(CLOCK_MONOTONIC), wake = probe_at;
        if (wanted) {
            lights_tick(&lights, now);
            display_tick(&display, now);
            unsigned char levels[4] = {0};
            int spectrum = plum_receiver_spectrum(levels);
            display_spectrum(&display, levels, spectrum, now);
            lights_spectrum(&lights, levels, spectrum, now);
            if (lights.active)
                earliest(&wake, lights.next_frame);
            if (display.active) {
                earliest(&wake, display.check_at);
                if (display.mode == DISPLAY_SPECTRUM)
                    earliest(&wake, display.next_frame);
            } else if (spectrum)
                earliest(&wake, display.yield_until > now ? display.yield_until : now + 50000);
            if (grabbed) {
                int64_t gesture = gesture_deadline(&g);
                if (gesture)
                    earliest(&wake, gesture - now_us(event_clock) + now);
            }
        } else {
            lights_tick(&lights, now);
            if (lights.active)
                earliest(&wake, lights.next_frame);
            if (display.mode == DISPLAY_ANIMATION) {
                display_tick(&display, now);
                if (display.active)
                    earliest(&wake, display.check_at);
            } else
                display_close(&display);
        }
        int64_t wait = wake - now_us(CLOCK_MONOTONIC);
        int timeout = wait <= 0 ? 0 : wait >= 300000 ? 300 : (int)((wait + 999) / 1000);
        struct pollfd pfd = {fd, POLLIN, 0};
        poll(&pfd, fd >= 0 ? 1 : 0, timeout);
    }
    if (grabbed)
        ioctl(fd, EVIOCGRAB, 0);
    lights_close(&lights, 1);
    display_close(&display);
    return 0;
}
#endif
