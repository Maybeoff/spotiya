struct ring_frame {
    unsigned char level[9];
    unsigned short ms;
};
struct ring_animation {
    const struct ring_frame *frames;
    unsigned count;
};
struct display_frame {
    unsigned char level[34];
    unsigned short ms;
};
struct display_animation {
    const struct display_frame *frames;
    unsigned count;
};
struct mini2_config {
    const char *input, *ring_prefix, *display_prefix;
    unsigned alice, volume_up, volume_down;
    struct ring_animation next, previous, pause, resume;
    struct display_animation clock_next, clock_previous, clock_pause;
    struct ring_animation volume;
};
static struct mini2_config control_config = {"/dev/input/event2",
                                             "/sys/class/leds/plum:",
                                             "/sys/class/leds/plum:white:clock",
                                             582,
                                             115,
                                             114,
                                             {0, 0},
                                             {0, 0},
                                             {0, 0},
                                             {0, 0},
                                             {0, 0},
                                             {0, 0},
                                             {0, 0},
                                             {0, 0}};
