#include <stdio.h>
#include "buffer.h"

typedef struct RingBuffer RingBuffer;

/* One fixed-size ring buffer. */
struct RingBuffer {
    int *data;
    int head;
    int len;
};

enum BufferState {
    BUFFER_OK,
    BUFFER_FULL
};

int rb_push(RingBuffer *rb, int value);

static int rb_wrap(int i, int len) {
    return i % len;
}

int rb_push(RingBuffer *rb, int value) {
    int slot = rb_wrap(rb->head, rb->len);
    rb->data[slot] = value;
    rb->head = slot + 1;
    printf("pushed %d\n", value);
    return BUFFER_OK;
}
