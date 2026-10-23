#define _POSIX_C_SOURCE 200809L
#include <stddef.h>
#include <stdint.h>
#include <string.h>

enum { VOL_DOWN = 114, VOL_UP = 115, ALICE = 582 };
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
    return code == VOL_DOWN ? 0 : code == VOL_UP ? 1 : code == ALICE ? 2 : -1;
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
extern int mini2_receiver_status(int *paused, int *volume);
extern int mini2_receiver_command(int action);
extern int mini2_receiver_running(void);
extern int mini2_receiver_spectrum(unsigned char *levels);
static int spotify_active(void) { return mini2_receiver_status(&current_paused, &current_volume); }
static int command(enum action a) { return mini2_receiver_command((int)a); }
static int empty_keys(const unsigned char *keys) {
    for (int i = 0; i < 96; i++)
        if (keys[i])
            return 0;
    return 1;
}
int mini2_buttons_run(void) {
    int fd = open("/dev/input/event0", O_RDONLY | O_NONBLOCK);
    if (fd < 0) {
        note("buttons: input open failed\n");
        return 1;
    }
    struct gesture g = {0};
    struct lights lights = {0};
    struct display display = {0};
    unsigned char held[96] = {0}, actual[96];
    int grabbed = 0, wanted = 0;
    int64_t probe_at = 0;
    struct input_event_abi events[64];
    note("buttons: ready (Alice control until Spotify is active)\n");
    while (running) {
        if (!mini2_receiver_running())
            break;
        int64_t mono = now_us(CLOCK_MONOTONIC);
        if (mono >= probe_at) {
            int previous_volume = current_volume, was_active = wanted;
            wanted = spotify_active();
            probe_at = now_us(CLOCK_MONOTONIC) + 300000;
            if (wanted && was_active && current_volume != previous_volume)
                display_show(&display, current_steps > 0 ? current_volume * 100 / current_steps : 0,
                             now_us(CLOCK_MONOTONIC));
            if (!wanted) {
                queued = 0;
                lights_close(&lights, 1);
                display_close(&display);
            }
        }
        ssize_t n;
        while ((n = read(fd, events, sizeof events)) > 0) {
            if (n % (ssize_t)sizeof events[0]) {
                note("buttons: input ABI mismatch\n");
                running = 0;
                break;
            }
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
            /* EVIOCGKEY returns the bitmap length on success, not zero. */
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
        if (grabbed && wanted)
            tick(&g, now_us(CLOCK_REALTIME));
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
                if (wanted) {
                    lights_begin(&lights, queue[i], feedback_paused, current_volume, current_steps,
                                 now);
                    if (queue[i] == LOUDER || queue[i] == QUIETER)
                        display_show(&display,
                                     current_steps > 0 ? current_volume * 100 / current_steps : 0,
                                     now);
                }
            } else {
                note(" failed; returning controls\n");
                wanted = 0;
                break;
            }
        }
        queued = 0;
        if (wanted) {
            lights_tick(&lights, now_us(CLOCK_MONOTONIC));
            display_tick(&display, now_us(CLOCK_MONOTONIC));
            unsigned char levels[4] = {0};
            int spectrum = mini2_receiver_spectrum(levels);
            display_spectrum(&display, levels, spectrum, now_us(CLOCK_MONOTONIC));
        } else {
            lights_close(&lights, 1);
            display_close(&display);
        }
        struct pollfd pfd = {fd, POLLIN, 0};
        poll(&pfd, 1, 10);
    }
    if (grabbed)
        ioctl(fd, EVIOCGRAB, 0);
    lights_close(&lights, 1);
    display_close(&display);
    close(fd);
    return 0;
}
#endif
