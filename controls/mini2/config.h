struct mini2_config {
    const char *input, *ring_prefix, *display_prefix;
    unsigned alice, volume_up, volume_down;
};
static struct mini2_config control_config = {"/dev/input/event0",
                                             "/sys/class/leds/mini2:",
                                             "/sys/class/leds/mini2:white:clock",
                                             582,
                                             115,
                                             114};
