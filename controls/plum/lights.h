/* Short, cooperative ring effects. Never claim a ring already used by Alice. */
#ifndef RING_PREFIX
#define RING_PREFIX control_config.ring_prefix
#endif
enum { PLUM_RING_LEDS = 9 };
struct lights {
    int fd[PLUM_RING_LEDS], active, scene, paused, volume, steps;
    const struct ring_animation *animation;
    unsigned char last[PLUM_RING_LEDS];
    int64_t started, next_frame, yield_until;
};
static int ring_read(int fd) {
    char text[16];
    if (lseek(fd, 0, SEEK_SET) < 0)
        return -1;
    ssize_t n = read(fd, text, sizeof text);
    if (n <= 0)
        return -1;
    int value = 0, digits = 0;
    for (ssize_t i = 0; i < n && text[i] >= '0' && text[i] <= '9'; i++) {
        value = value * 10 + text[i] - '0';
        digits++;
        if (value > 255)
            return -1;
    }
    return digits ? value : -1;
}
static int ring_write(int fd, int value) {
    char text[4], *end = text + sizeof text, *p = end;
    *--p = '\n';
    do {
        *--p = (char)('0' + value % 10);
        value /= 10;
    } while (value);
    return lseek(fd, 0, SEEK_SET) >= 0 && write(fd, p, (size_t)(end - p)) == end - p;
}
static int lights_owned(struct lights *l) {
    for (int i = 0; i < PLUM_RING_LEDS; i++)
        if (ring_read(l->fd[i]) != l->last[i])
            return 0;
    return 1;
}
static void lights_close(struct lights *l, int restore) {
    if (!l->active)
        return;
    if (restore && lights_owned(l))
        for (int i = 0; i < PLUM_RING_LEDS; i++)
            if (l->last[i])
                (void)ring_write(l->fd[i], 0);
    for (int i = 0; i < PLUM_RING_LEDS; i++)
        close(l->fd[i]);
    l->active = 0;
}
static int lights_open(struct lights *l) {
    if (!RING_PREFIX)
        return 0;
    static const char *colors[] = {"red:ring1/brightness", "green:ring1/brightness",
                                   "blue:ring1/brightness"};
    int count = 0;
    for (int color = 0; color < 3; color++)
        for (int segment = 0; segment < 3; segment++) {
            char path[128];
            size_t n = strlen(RING_PREFIX);
            const char *c = colors[color];
            if (n + strlen(c) + 1 > sizeof path)
                goto fail;
            memcpy(path, RING_PREFIX, n);
            strcpy(path + n, c);
            char *digit = strstr(path + n, "ring1");
            if (!digit)
                goto fail;
            digit[4] = (char)('1' + segment);
            int fd = open(path, O_RDWR | O_CLOEXEC);
            if (fd < 0)
                goto fail;
            l->fd[count++] = fd;
            if (ring_read(fd) != 0)
                goto fail;
        }
    memset(l->last, 0, sizeof l->last);
    l->active = 1;
    return 1;
fail:
    for (int i = 0; i < count; i++)
        close(l->fd[i]);
    return 0;
}
static void lights_begin(struct lights *l, enum action a, int paused, int volume, int steps,
                         int64_t now) {
    if (now < l->yield_until)
        return;
    if (l->active && !lights_owned(l)) {
        lights_close(l, 0);
        l->yield_until = now + 1000000;
        return;
    }
    if (!l->active && !lights_open(l))
        return;
    l->scene = a;
    l->paused = paused;
    const struct ring_animation *stock =
        a == NEXT     ? &control_config.next
        : a == PREV   ? &control_config.previous
        : a == TOGGLE ? (paused ? &control_config.pause : &control_config.resume)
                      : NULL;
    l->animation = stock && stock->count ? stock : NULL;
    l->volume = volume;
    l->steps = steps;
    l->started = now;
    l->next_frame = now;
}
static void lights_tick(struct lights *l, int64_t now) {
    if (!l->active || now < l->next_frame)
        return;
    if (!lights_owned(l)) {
        lights_close(l, 0);
        l->yield_until = now + 1000000;
        return;
    }
    int elapsed = (int)((now - l->started) / 1000);
    const struct ring_animation *palette = &control_config.volume;
    if ((l->scene == LOUDER || l->scene == QUIETER) && palette->count && l->steps > 0) {
        enum { HOLD_MS = 550, FADE_MS = 420 };
        if (elapsed >= HOLD_MS + FADE_MS) {
            lights_close(l, 1);
            return;
        }
        int volume = l->volume < 0 ? 0 : l->volume > l->steps ? l->steps : l->volume;
        unsigned index = (unsigned)((volume * (int)(palette->count - 1) + l->steps / 2) / l->steps);
        int left = elapsed < HOLD_MS ? FADE_MS : HOLD_MS + FADE_MS - elapsed;
        for (int i = 0; i < PLUM_RING_LEDS; i++) {
            int value = palette->frames[index].level[i] * left / FADE_MS;
            if (value != l->last[i]) {
                if (!ring_write(l->fd[i], value)) {
                    lights_close(l, 1);
                    return;
                }
                l->last[i] = (unsigned char)value;
            }
        }
        l->next_frame = elapsed < HOLD_MS ? l->started + HOLD_MS * 1000 : now + 20000;
        return;
    }
    if (l->animation) {
        int end = 0;
        const struct ring_frame *frame = NULL;
        for (unsigned i = 0; i < l->animation->count; i++) {
            end += l->animation->frames[i].ms;
            if (elapsed < end) {
                frame = &l->animation->frames[i];
                break;
            }
        }
        if (!frame) {
            lights_close(l, 1);
            return;
        }
        for (int i = 0; i < PLUM_RING_LEDS; i++) {
            int value = frame->level[i];
            if (value != l->last[i]) {
                if (!ring_write(l->fd[i], value)) {
                    lights_close(l, 1);
                    return;
                }
                l->last[i] = (unsigned char)value;
            }
        }
        l->next_frame = l->started + (int64_t)end * 1000;
        return;
    }
    lights_close(l, 1);
}

static void lights_spectrum(struct lights *l, const unsigned char *levels, int enabled,
                             int64_t now) {
    if (!enabled) {
        if (l->active && l->scene == 99) {
            lights_close(l, 0);
        }
        return;
    }
    if (now < l->yield_until)
        return;
    if (l->active && l->scene != 99 && l->scene != 0) {
        if (lights_owned(l))
            return;
        lights_close(l, 0);
        l->yield_until = now + 1000000;
        return;
    }
    if (!l->active) {
        if (!lights_open(l)) {
            l->yield_until = now + 1000000;
            return;
        }
    }
    unsigned char frame[PLUM_RING_LEDS] = {0};
    int avg = 0;
    if (levels) {
        avg = (levels[0] + levels[1] + levels[2] + levels[3]) / 4;
        if (avg < 0) avg = 0;
        if (avg > 100) avg = 100;
    }
    int b = avg * 255 / 100;
    for (int i = 0; i < PLUM_RING_LEDS; i++) {
        frame[i] = (unsigned char)b;
    }
    for (int i = 0; i < PLUM_RING_LEDS; i++) {
        if (frame[i] != l->last[i]) {
            if (!ring_write(l->fd[i], frame[i])) {
                lights_close(l, 0);
                return;
            }
            l->last[i] = frame[i];
        }
    }
    l->scene = 99;
    l->next_frame = now + 40000;
}
