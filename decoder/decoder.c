#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>
#include <unicorn/unicorn.h>
#include <unicorn/x86.h>

#include "resources.h"

#define ARRAY_SIZE(x) (sizeof(x) / sizeof((x)[0]))
#define STACK UINT64_C(0x300f0000)
#define RETURN_PC UINT64_C(0x2000f000)
#define CONTEXT UINT64_C(0x20003000)
#define OPAQUE UINT64_C(0x20003400)
#define SOURCE UINT64_C(0x20003500)
#define DESTINATION UINT64_C(0x2000a000)
#define MAX_CHUNK 24576
#define HEAP_END UINT64_C(0x21200000)
_Static_assert(SOURCE + MAX_CHUNK <= DESTINATION, "input/output buffers overlap");
_Static_assert(DESTINATION + MAX_CHUNK <= UINT64_C(0x20010000), "output exceeds data mapping");

struct tls_entry { uint64_t control, value; };
struct engine {
    uc_engine *uc;
    uint64_t heap;
    uint32_t random;
    uint64_t code_byte_budget;
    struct tls_entry tls[32];
    size_t tls_count;
    int current_site;
    uint64_t saved[7];
    bool failed;
    uint8_t scratch[0x20000];
};

static const int saved_regs[] = {
    UC_X86_REG_RSP, UC_X86_REG_RBP, UC_X86_REG_RBX, UC_X86_REG_R12,
    UC_X86_REG_R13, UC_X86_REG_R14, UC_X86_REG_R15
};

static bool fail(struct engine *e, const char *operation, uc_err error) {
    if (!e->failed)
        fprintf(stderr, "decoder: %s: %s\n", operation, uc_strerror(error));
    e->failed = true;
    uc_emu_stop(e->uc);
    return false;
}

static bool memory_read(struct engine *e, uint64_t address, void *out, size_t size) {
    uc_err error = uc_mem_read(e->uc, address, out, size);
    return error == UC_ERR_OK || fail(e, "read", error);
}

static bool memory_write(struct engine *e, uint64_t address, const void *data, size_t size) {
    uc_err error = uc_mem_write(e->uc, address, data, size);
    return error == UC_ERR_OK || fail(e, "write", error);
}

static uint64_t reg(struct engine *e, int id) {
    uint64_t value = 0;
    uc_err error = uc_reg_read(e->uc, id, &value);
    if (error != UC_ERR_OK) fail(e, "register read", error);
    return value;
}

static void put_reg(struct engine *e, int id, uint64_t value) {
    uc_err error = uc_reg_write(e->uc, id, &value);
    if (error != UC_ERR_OK) fail(e, "register write", error);
}

static void guest_return(struct engine *e, bool set_result, uint64_t result) {
    uint64_t stack = reg(e, UC_X86_REG_RSP), target;
    if (!memory_read(e, stack, &target, sizeof(target))) return;
    if (set_result) put_reg(e, UC_X86_REG_RAX, result);
    put_reg(e, UC_X86_REG_RSP, stack + 8);
    put_reg(e, UC_X86_REG_RIP, target);
}

static uint64_t allocate(struct engine *e, uint64_t length, uint64_t alignment) {
    if (length > sizeof(e->scratch) || alignment < 16 || alignment > 0x10000 || (alignment & (alignment - 1))) {
        fail(e, "allocation shape", UC_ERR_ARG);
        return 0;
    }
    uint64_t pointer = (e->heap + alignment - 1) & ~(alignment - 1);
    if (pointer + length > HEAP_END) {
        fail(e, "heap bound", UC_ERR_NOMEM);
        return 0;
    }
    e->heap = (pointer + (length ? length : 16) + 15) & ~UINT64_C(15);
    memset(e->scratch, 0, (size_t)length);
    if (length && !memory_write(e, pointer, e->scratch, (size_t)length)) return 0;
    return pointer;
}

static bool copy_memory(struct engine *e, uint64_t dst, uint64_t src, uint64_t size) {
    if (size > sizeof(e->scratch)) return fail(e, "copy bound", UC_ERR_ARG);
    return memory_read(e, src, e->scratch, (size_t)size) && memory_write(e, dst, e->scratch, (size_t)size);
}

static bool put_string(struct engine *e, uint64_t dst, const uint8_t *data, size_t length) {
    if (length > 4096) return fail(e, "string bound", UC_ERR_ARG);
    uint8_t header[24] = {0};
    if (length < 23) {
        header[0] = (uint8_t)(length * 2);
        memcpy(header + 1, data, length);
    } else {
        uint64_t pointer = allocate(e, length + 1, 16);
        uint64_t fields[] = {(length + 1) | 1, length, pointer};
        if (!pointer || !memory_write(e, pointer, data, length)) return false;
        memcpy(header, fields, sizeof(header));
    }
    return memory_write(e, dst, header, sizeof(header));
}

static void track_call(struct engine *e, uint64_t pc) {
    size_t lo = 0, hi = ARRAY_SIZE(tracked_calls);
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        if (tracked_calls[mid].pc < pc) lo = mid + 1;
        else hi = mid;
    }
    if (lo == ARRAY_SIZE(tracked_calls) || tracked_calls[lo].pc != pc) return;
    e->current_site = (int)tracked_calls[lo].site;
    for (size_t i = 0; i < ARRAY_SIZE(saved_regs); ++i) e->saved[i] = reg(e, saved_regs[i]);
}

static void code_hook(uc_engine *uc, uint64_t pc, uint32_t instruction_size, void *user) {
    (void)uc; (void)instruction_size;
    struct engine *e = user;
    if (pc >= 0xbe9085 && pc < 0xbe9bd3) track_call(e, pc);
    uint64_t a, b, n;
    switch (pc) {
    case 0x18b5800: case 0x18b57f0: case 0x18b56a0: case 0x18b8310:
        guest_return(e, true, 0); return;
    case 0x18b57e0:
        e->random = 1664525U * e->random + 1013904223U;
        guest_return(e, true, e->random); return;
    case 0x18b5560: {
        uint8_t data[4096];
        n = reg(e, UC_X86_REG_RDX);
        if (n > sizeof(data)) { fail(e, "string init bound", UC_ERR_ARG); return; }
        if (!memory_read(e, reg(e, UC_X86_REG_RSI), data, (size_t)n)) return;
        if (!put_string(e, reg(e, UC_X86_REG_RDI), data, (size_t)n)) return;
        guest_return(e, false, 0); return;
    }
    case 0x18b5600: {
        uint8_t header[24], data[4096];
        if (!memory_read(e, reg(e, UC_X86_REG_RSI), header, sizeof(header))) return;
        if (header[0] & 1) {
            uint64_t fields[3]; memcpy(fields, header, sizeof(fields));
            n = fields[1];
            if (n > sizeof(data)) { fail(e, "string copy bound", UC_ERR_ARG); return; }
            if (!memory_read(e, fields[2], data, (size_t)n)) return;
        } else {
            n = header[0] >> 1;
            if (n > 23) { fail(e, "small string shape", UC_ERR_ARG); return; }
            memcpy(data, header + 1, (size_t)n);
        }
        if (!put_string(e, reg(e, UC_X86_REG_RDI), data, (size_t)n)) return;
        guest_return(e, false, 0); return;
    }
    case 0x18b5550: case 0x18b55e0: case 0x18b57c0: case 0xafc5a8:
        guest_return(e, false, 0); return;
    case 0x18b55d0:
        a = reg(e, UC_X86_REG_RDI);
        for (n = 0; n < 4096; ++n) {
            uint8_t byte;
            if (!memory_read(e, a + n, &byte, 1)) return;
            if (!byte) { guest_return(e, true, n); return; }
        }
        fail(e, "strlen bound", UC_ERR_ARG); return;
    case 0x18b8210: {
        a = reg(e, UC_X86_REG_RDI);
        for (size_t i = 0; i < e->tls_count; ++i) {
            if (e->tls[i].control == a) { guest_return(e, true, e->tls[i].value); return; }
        }
        if (e->tls_count == ARRAY_SIZE(e->tls)) { fail(e, "TLS count bound", UC_ERR_ARG); return; }
        uint64_t fields[4];
        if (!memory_read(e, a, fields, sizeof(fields))) return;
        if (fields[0] > 0x10000) { fail(e, "TLS size bound", UC_ERR_ARG); return; }
        b = allocate(e, fields[0], fields[1] < 16 ? 16 : fields[1]);
        if (!b || (fields[3] && !copy_memory(e, b, fields[3], fields[0]))) return;
        e->tls[e->tls_count++] = (struct tls_entry){a, b};
        guest_return(e, true, b); return;
    }
    case 0x18b6230:
        a = reg(e, UC_X86_REG_RDI); b = reg(e, UC_X86_REG_RSI);
        if (a && b > UINT64_MAX / a) { fail(e, "calloc overflow", UC_ERR_ARG); return; }
        n = allocate(e, a * b, 16);
        if (!n) return;
        guest_return(e, true, n); return;
    case 0x18b55c0:
        n = allocate(e, reg(e, UC_X86_REG_RDI), 16);
        if (!n) return;
        guest_return(e, true, n); return;
    case 0x18b7160: case 0x18b7170:
        a = reg(e, UC_X86_REG_RDI);
        if (!copy_memory(e, a, reg(e, UC_X86_REG_RSI), reg(e, UC_X86_REG_RDX))) return;
        guest_return(e, true, a); return;
    case 0x18b5570:
        guest_return(e, true, 0x20002000); return;
    case 0x18b5530:
        guest_return(e, true, reg(e, UC_X86_REG_RDI)); return;
    case 0x18b5c60:
        guest_return(e, false, 0); return;
    case 0x18b55a0: {
        if (e->current_site < 0) { fail(e, "throw without catch state", UC_ERR_ARG); return; }
        const struct catch_site *site = &catch_sites[e->current_site];
        a = reg(e, UC_X86_REG_RDI); b = reg(e, UC_X86_REG_RSI);
        for (size_t i = site->begin; i < site->begin + site->count; ++i) {
            uint64_t type;
            if (!memory_read(e, catch_entries[i].type_address, &type, sizeof(type))) return;
            if (type != b) continue;
            for (size_t j = 0; j < ARRAY_SIZE(saved_regs); ++j) put_reg(e, saved_regs[j], e->saved[j]);
            put_reg(e, UC_X86_REG_RAX, a);
            put_reg(e, UC_X86_REG_RDX, catch_entries[i].selector);
            put_reg(e, UC_X86_REG_RIP, site->landing);
            return;
        }
        fail(e, "unmatched catch type", UC_ERR_ARG); return;
    }
    default:
        if (pc >= 0x18b5500 && pc < 0x18b8460) {
            fprintf(stderr, "decoder: unsupported external call at 0x%" PRIx64 "\n", pc);
            fail(e, "external call", UC_ERR_ARG);
        }
    }
}

static bool invalid_memory(uc_engine *uc, uc_mem_type access, uint64_t address, int size, int64_t value, void *user) {
    (void)uc; (void)access; (void)size; (void)value;
    struct engine *e = user;
    fprintf(stderr, "decoder: missing emulated page at 0x%" PRIx64 "\n", address);
    fail(e, "memory bound", UC_ERR_MAP);
    return false;
}

static void block_hook(uc_engine *uc, uint64_t address, uint32_t size, void *user) {
    (void)uc; (void)address;
    struct engine *e = user;
    if (size > e->code_byte_budget) { fail(e, "execution budget", UC_ERR_RESOURCE); return; }
    e->code_byte_budget -= size;
}

static bool run(struct engine *e, uint64_t begin, uint64_t end, size_t instructions) {
    e->code_byte_budget = (uint64_t)instructions * 15;
    uc_err error = uc_emu_start(e->uc, begin, end, 0, 0);
    if (e->failed || error != UC_ERR_OK) return fail(e, "emulation", error);
    if (reg(e, UC_X86_REG_RIP) != end) return fail(e, "instruction bound", UC_ERR_RESOURCE);
    return !e->failed;
}

static bool invoke(struct engine *e, uint64_t address, const uint64_t *args, size_t count) {
    const int registers[] = {UC_X86_REG_RDI, UC_X86_REG_RSI, UC_X86_REG_RDX, UC_X86_REG_RCX, UC_X86_REG_R8, UC_X86_REG_R9};
    uint64_t stop = RETURN_PC;
    if (!memory_write(e, STACK, &stop, sizeof(stop))) return false;
    put_reg(e, UC_X86_REG_RSP, STACK);
    for (size_t i = 0; i < count; ++i) put_reg(e, registers[i], args[i]);
    return run(e, address, RETURN_PC, 10000000);
}

static bool initialize(struct engine *e) {
    uc_err error = uc_open(UC_ARCH_X86, UC_MODE_64, &e->uc);
    if (error != UC_ERR_OK) { fprintf(stderr, "decoder: uc_open: %s\n", uc_strerror(error)); return false; }
    e->heap = 0x21000000; e->random = 0x12345678; e->current_site = -1;
    error = uc_ctl_set_tcg_buffer_size(e->uc, 2U * 1024U * 1024U);
    if (error != UC_ERR_OK) return fail(e, "JIT buffer", error);
    for (size_t i = 0; i < ARRAY_SIZE(image_pages); ++i) {
        error = uc_mem_map(e->uc, image_pages[i], 4096, UC_PROT_ALL);
        if (error != UC_ERR_OK) return fail(e, "image map", error);
        if (!memory_write(e, image_pages[i], initial_image + 4096 * i, 4096)) return false;
    }
    const uint64_t regions[][2] = {{0x20000000,0x10000},{0x21000000,0x200000},{0x30000000,0x100000},{0x40000000,0x1000}};
    for (size_t i = 0; i < ARRAY_SIZE(regions); ++i) {
        error = uc_mem_map(e->uc, regions[i][0], regions[i][1], UC_PROT_ALL);
        if (error != UC_ERR_OK) return fail(e, "runtime map", error);
    }
    put_reg(e, UC_X86_REG_FS_BASE, 0x40000000);
    uc_hook hook;
    const uint64_t hooks[][2] = {{0xbe9085,0xbe9bd2},{0x18b5500,0x18b845f},{0xafc5a8,0xafc5a8}};
    for (size_t i = 0; i < ARRAY_SIZE(hooks); ++i) {
        error = uc_hook_add(e->uc, &hook, UC_HOOK_CODE, (void *)code_hook, e, hooks[i][0], hooks[i][1]);
        if (error != UC_ERR_OK) return fail(e, "code hook", error);
    }
    error = uc_hook_add(e->uc, &hook, UC_HOOK_BLOCK, (void *)block_hook, e, 1, 0);
    if (error != UC_ERR_OK) return fail(e, "budget hook", error);
    error = uc_hook_add(e->uc, &hook, UC_HOOK_MEM_INVALID, (void *)invalid_memory, e, 1, 0);
    return error == UC_ERR_OK || fail(e, "memory hook", error);
}

static bool setup_key(struct engine *e, const uint8_t key[16], const uint8_t extra[4]) {
    uint8_t string[24] = {32}, opaque[28], context[0x310] = {0};
    memset(e->scratch, 0, 0x5000);
    if (!memory_write(e, STACK - 0x2000, e->scratch, 0x5000) || !memory_write(e, 0x20000000, e->scratch, 0x1000)) return false;
    e->current_site = -1;
    memcpy(string + 1, decoder_constant, 16);
    if (!memory_write(e, STACK + 0xd0, key, 16) || !memory_write(e, STACK + 0x5c, extra, 4) || !memory_write(e, STACK + 0x40, string, sizeof(string))) return false;
    put_reg(e, UC_X86_REG_RSP, STACK);
    put_reg(e, UC_X86_REG_RAX, STACK + 0x41);
    put_reg(e, UC_X86_REG_RBX, 0);
    put_reg(e, UC_X86_REG_R13, 0x20000000);
    if (!run(e, 0xbe9085, 0xbe9bd3, 20000000)) return false;
    if (!memory_read(e, STACK + 0xe0, opaque, sizeof(opaque)) || !memory_write(e, OPAQUE, opaque, sizeof(opaque)) || !memory_write(e, CONTEXT, context, sizeof(context))) return false;
    const uint64_t args[] = {CONTEXT, OPAQUE};
    return invoke(e, 0x143f7be, args, ARRAY_SIZE(args));
}

static bool decrypt_range(struct engine *e, const uint8_t *input, uint8_t *output, size_t length, uint64_t offset) {
    if (!memory_write(e, SOURCE, input, length)) return false;
    const uint64_t args[] = {CONTEXT, offset, DESTINATION, SOURCE, length};
    return invoke(e, 0x143f8fe, args, ARRAY_SIZE(args)) && memory_read(e, DESTINATION, output, length);
}

static bool read_file(const char *path, uint8_t *out, size_t capacity, size_t *length) {
    FILE *file = fopen(path, "rb");
    if (!file) { perror("decoder: input file"); return false; }
    *length = fread(out, 1, capacity, file);
    bool ok = !ferror(file) && fgetc(file) == EOF;
    fclose(file);
    if (!ok) fprintf(stderr, "decoder: input exceeds its size bound\n");
    return ok;
}

static void destroy(struct engine *e) {
    if (!e) return;
    if (e->uc) uc_close(e->uc);
    free(e);
}

static uint32_t load_le32(const uint8_t *data) {
    return (uint32_t)data[0] | (uint32_t)data[1] << 8 | (uint32_t)data[2] << 16 | (uint32_t)data[3] << 24;
}

static bool reply(uint8_t status, const uint8_t *data, size_t length) {
    uint32_t frame_size = (uint32_t)length + 1;
    uint8_t header[] = {(uint8_t)frame_size, (uint8_t)(frame_size >> 8), (uint8_t)(frame_size >> 16), (uint8_t)(frame_size >> 24), status};
    return fwrite(header, 1, sizeof(header), stdout) == sizeof(header) &&
           (!length || fwrite(data, 1, length, stdout) == length) && !fflush(stdout);
}

static int serve(void) {
    if (isatty(STDIN_FILENO) || isatty(STDOUT_FILENO)) {
        fprintf(stderr, "decoder: framed input/output require pipes\n"); return 2;
    }
    struct engine *engine = NULL;
    uint8_t header[4], frame[MAX_CHUNK + 9], output[MAX_CHUNK];
    int result = 0;
    for (;;) {
        size_t count = fread(header, 1, sizeof(header), stdin);
        if (!count && feof(stdin)) break;
        if (count != sizeof(header)) { result = 2; break; }
        uint32_t length = load_le32(header);
        if (!length || length > sizeof(frame) || fread(frame, 1, length, stdin) != length) { result = 2; break; }
        switch (frame[0]) {
        case 1:
            if (length != 21) { destroy(engine); engine = NULL; if (!reply(2, NULL, 0)) result = 1; break; }
            destroy(engine);
            engine = calloc(1, sizeof(*engine));
            if (!engine || !initialize(engine) || !setup_key(engine, frame + 1, frame + 17)) {
                reply(1, NULL, 0); result = 1; goto done;
            }
            if (!reply(0, NULL, 0)) { result = 1; goto done; }
            break;
        case 2: {
            if (!engine || length < 10 || length > MAX_CHUNK + 9) { if (!reply(2, NULL, 0)) result = 1; break; }
            uint64_t offset = load_le32(frame + 1) | (uint64_t)load_le32(frame + 5) << 32;
            if (offset > INT64_MAX) { if (!reply(2, NULL, 0)) result = 1; break; }
            if (!decrypt_range(engine, frame + 9, output, length - 9, offset)) {
                reply(1, NULL, 0); result = 1; goto done;
            }
            if (!reply(0, output, length - 9)) { result = 1; goto done; }
            break;
        }
        case 3:
            if (length != 1) { if (!reply(2, NULL, 0)) result = 1; break; }
            if (!reply(0, NULL, 0)) result = 1;
            goto done;
        case 4: {
            if (length != 1) { if (!reply(2, NULL, 0)) result = 1; break; }
            uint8_t configuration[20] = {5,0,0,0};
            memcpy(configuration + 4, playplay_build_token, sizeof(playplay_build_token));
            if (!reply(0, configuration, sizeof(configuration))) { result = 1; goto done; }
            break;
        }
        default:
            if (!reply(2, NULL, 0)) result = 1;
        }
        if (result) break;
    }
done:
    destroy(engine);
    return result;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--serve") == 0) return serve();
    if (argc == 2 && strcmp(argv[1], "--info") == 0) {
        printf("{\"static_image_bytes\":%zu,\"mapped_pages\":%zu,\"jit_buffer_bytes\":2097152,\"protocol_version\":1,\"network\":false}\n", sizeof(initial_image), ARRAY_SIZE(image_pages));
        return 0;
    }
    if ((argc != 6 && argc != 7) || strcmp(argv[1], "--decode-range") || (argc == 7 && strcmp(argv[6], "--stats"))) {
        fprintf(stderr, "Usage: %s --decode-range KEY16 EXTRA4 CIPHERTEXT OFFSET [--stats]\n", argv[0]);
        return 2;
    }
    if (isatty(STDOUT_FILENO)) { fprintf(stderr, "decoder: binary output requires a pipe\n"); return 2; }
    char *end;
    errno = 0;
    uint64_t offset = strtoull(argv[5], &end, 10);
    if (errno || !*argv[5] || *argv[5] == '-' || *end || offset > INT64_MAX) { fprintf(stderr, "decoder: invalid byte offset\n"); return 2; }
    uint8_t key[16], extra[4], input[MAX_CHUNK], output[MAX_CHUNK];
    size_t key_length, extra_length, length;
    if (!read_file(argv[2], key, sizeof(key), &key_length) || key_length != sizeof(key) ||
        !read_file(argv[3], extra, sizeof(extra), &extra_length) || extra_length != sizeof(extra) ||
        !read_file(argv[4], input, sizeof(input), &length) || !length) {
        fprintf(stderr, "decoder: invalid bounded input shape\n"); return 2;
    }
    struct timespec begin, finish;
    clock_gettime(CLOCK_MONOTONIC, &begin);
    struct engine *engine = calloc(1, sizeof(*engine));
    if (!engine) return 1;
    bool ok = initialize(engine) && setup_key(engine, key, extra) && decrypt_range(engine, input, output, length, offset);
    destroy(engine);
    if (!ok) return 1;
    if (fwrite(output, 1, length, stdout) != length || fflush(stdout)) return 1;
    clock_gettime(CLOCK_MONOTONIC, &finish);
    if (argc == 7) {
        struct rusage usage;
        getrusage(RUSAGE_SELF, &usage);
        double milliseconds = (finish.tv_sec - begin.tv_sec) * 1000.0 + (finish.tv_nsec - begin.tv_nsec) / 1e6;
        fprintf(stderr, "{\"elapsed_ms\":%.3f,\"max_rss_kib\":%ld,\"decoded_bytes\":%zu}\n", milliseconds, usage.ru_maxrss, length);
    }
    return 0;
}
