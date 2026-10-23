/* Shared display overlays yield to native animations and preserve the clock. */
#ifndef DISPLAY_PREFIX
#define DISPLAY_PREFIX control_config.display_prefix
#endif
enum { DISPLAY_LEDS = 34 };
enum { DISPLAY_VOLUME = 1, DISPLAY_SPECTRUM = 2 };
struct display {
    int fd[DISPLAY_LEDS], active, brightness, mode;
    unsigned char saved[DISPLAY_LEDS], last[DISPLAY_LEDS];
    int64_t until, check_at, next_frame, yield_until;
};
static void display_close(struct display *d) {
    if (!d->active)
        return;
    for (int i = 0; i < DISPLAY_LEDS; i++) {
        /* Restore only channels still carrying our last write. */
        if (ring_read(d->fd[i]) == d->last[i] && d->last[i] != d->saved[i])
            (void)ring_write(d->fd[i], d->saved[i]);
        close(d->fd[i]);
    }
    d->active = 0;
    d->mode = 0;
}
static int display_open(struct display *d) {
    if (!DISPLAY_PREFIX)
        return 0;
    static const char *extra[] = {"PLUS1", "PLUS2", "PLUS3", "PLUS4", "MINUS", "DEGREE"};
    int count = 0, brightness = 0;
    for (int i = 0; i < DISPLAY_LEDS; i++) {
        char path[128];
        size_t n = strlen(DISPLAY_PREFIX);
        if (n + 40 > sizeof path)
            goto fail;
        memcpy(path, DISPLAY_PREFIX, n);
        if (i < 28) {
            path[n++] = (char)('A' + i % 7);
            path[n++] = (char)('1' + i / 7);
        } else {
            size_t len = strlen(extra[i - 28]);
            memcpy(path + n, extra[i - 28], len);
            n += len;
        }
        strcpy(path + n, "/brightness");
        int fd = open(path, O_RDWR | O_CLOEXEC);
        if (fd < 0)
            goto fail;
        d->fd[count++] = fd;
        int value = ring_read(fd);
        if (value < 0 || value > 170)
            goto fail;
        d->saved[i] = d->last[i] = (unsigned char)value;
        if (value > brightness)
            brightness = value;
    }
    d->brightness = brightness ? brightness : 22;
    d->active = 1;
    return 1;
fail:
    for (int i = 0; i < count; i++)
        close(d->fd[i]);
    return 0;
}
static int display_unchanged(struct display *d) {
    for (int i = 0; i < DISPLAY_LEDS; i++) {
        int value = ring_read(d->fd[i]);
        if (value < 0)
            return 0;
        if (value == d->last[i])
            continue;
        /* The normal clock blinks its colon while our numbers are displayed.
         * Remember the latest colon state, then keep the overlay colon blank. */
        if (i == 28 || i == 29) {
            d->saved[i] = (unsigned char)value;
            if (!ring_write(d->fd[i], d->last[i]))
                return 0;
        } else
            return 0;
    }
    return 1;
}
static int display_is_clock(const unsigned char *frame) {
    static const unsigned char digits[] = {63, 6, 91, 79, 102, 109, 125, 7, 127, 111};
    int number[4] = {0}, blank = 1, first_blank = 0;
    for (int i = 30; i < DISPLAY_LEDS; i++)
        if (frame[i])
            return 0;
    for (int position = 0; position < 4; position++) {
        unsigned mask = 0;
        for (int segment = 0; segment < 7; segment++)
            if (frame[position * 7 + segment])
                mask |= 1u << segment;
        if (position == 0)
            first_blank = !mask;
        if (mask)
            blank = 0;
        number[position] = -1;
        for (int digit = 0; digit < 10; digit++)
            if (mask == digits[digit])
                number[position] = digit;
    }
    if (blank)
        return 1;
    if (first_blank)
        number[0] = 0;
    return number[0] >= 0 && number[1] >= 0 && number[2] >= 0 && number[3] >= 0 &&
           number[0] * 10 + number[1] < 24 && number[2] * 10 + number[3] < 60;
}
static void display_yield(struct display *d, int64_t now) {
    unsigned char current[DISPLAY_LEDS];
    int valid = 1;
    for (int i = 0; i < DISPLAY_LEDS; i++) {
        int value = ring_read(d->fd[i]);
        if (value < 0 || value > 170) {
            valid = 0;
            break;
        }
        current[i] = (unsigned char)value;
    }
    if (valid && display_is_clock(current)) {
        memcpy(d->saved, current, sizeof current);
        memcpy(d->last, current, sizeof current);
    }
    display_close(d);
    d->yield_until = now + 1500000;
}
static int display_frame(struct display *d, const unsigned char *frame) {
    for (int i = 0; i < DISPLAY_LEDS; i++)
        if (frame[i] != d->last[i]) {
            if (!ring_write(d->fd[i], frame[i])) {
                display_close(d);
                return 0;
            }
            d->last[i] = frame[i];
        }
    return 1;
}
static void display_show(struct display *d, int percent, int64_t now) {
    static const unsigned char digits[] = {63, 6, 91, 79, 102, 109, 125, 7, 127, 111};
    if (d->active && !display_unchanged(d)) {
        display_yield(d, now);
        return;
    }
    if (now < d->yield_until)
        return;
    if (!d->active && !display_open(d))
        return;
    if (percent < 0)
        percent = 0;
    if (percent > 100)
        percent = 100;
    unsigned char frame[DISPLAY_LEDS] = {0};
    for (int position = 3; position >= 1; position--) {
        unsigned mask = digits[percent % 10];
        for (int segment = 0; segment < 7; segment++)
            if (mask & (1u << segment))
                frame[position * 7 + segment] = (unsigned char)d->brightness;
        percent /= 10;
        if (!percent)
            break;
    }
    if (!display_frame(d, frame))
        return;
    d->mode = DISPLAY_VOLUME;
    d->until = now + 2000000;
    d->check_at = now + 50000;
}
static void display_tick(struct display *d, int64_t now) {
    if (!d->active || now < d->check_at)
        return;
    if (!display_unchanged(d)) {
        display_yield(d, now);
        return;
    }
    if (d->mode == DISPLAY_VOLUME && now >= d->until) {
        display_close(d);
        return;
    }
    d->check_at = now + 50000;
}
static int display_intensity(int value, int brightness) {
    if (value < 0)
        value = 0;
    if (value > 100)
        value = 100;
    return value * brightness / 100;
}
static void display_spectrum(struct display *d, const unsigned char *levels, int enabled,
                             int64_t now) {
    if (!enabled) {
        if (d->active && d->mode == DISPLAY_SPECTRUM)
            display_close(d);
        return;
    }
    if (now < d->yield_until || (d->active && d->mode == DISPLAY_VOLUME) ||
        (d->active && now < d->next_frame))
        return;
    if (!d->active) {
        if (!display_open(d)) {
            d->yield_until = now + 1000000;
            return;
        }
        if (!display_is_clock(d->saved)) {
            display_close(d);
            d->yield_until = now + 1500000;
            return;
        }
    }
    int brightness = d->brightness > 32 ? 32 : d->brightness;
    unsigned char frame[DISPLAY_LEDS] = {0};
    for (int column = 0; column < 4; column++) {
        int height = (levels[column] > 100 ? 100 : levels[column]) * 3;
        unsigned char *digit = frame + column * 7;
        digit[3] = (unsigned char)display_intensity(height, brightness);
        digit[2] = digit[4] = (unsigned char)display_intensity(height - 50, brightness);
        digit[6] = (unsigned char)display_intensity(height - 100, brightness);
        digit[1] = digit[5] = (unsigned char)display_intensity(height - 150, brightness);
        digit[0] = (unsigned char)display_intensity(height - 200, brightness);
    }
    if (!display_frame(d, frame))
        return;
    d->mode = DISPLAY_SPECTRUM;
    d->next_frame = now + 50000;
    d->check_at = now + 50000;
}
