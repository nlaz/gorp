/* Single-producer single-consumer ring buffer for the ingest hot path. */

#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

struct ring_buffer {
	uint8_t *storage;
	size_t capacity; /* always a power of two */
	_Atomic size_t head;
	_Atomic size_t tail;
};

static size_t round_up_pow2(size_t n)
{
	size_t p = 1;
	while (p < n)
		p <<= 1;
	return p;
}

struct ring_buffer *ring_buffer_create(size_t min_capacity)
{
	struct ring_buffer *rb = calloc(1, sizeof(*rb));
	if (!rb)
		return NULL;
	rb->capacity = round_up_pow2(min_capacity);
	rb->storage = malloc(rb->capacity);
	if (!rb->storage) {
		free(rb);
		return NULL;
	}
	atomic_store(&rb->head, 0);
	atomic_store(&rb->tail, 0);
	return rb;
}

void ring_buffer_destroy(struct ring_buffer *rb)
{
	if (!rb)
		return;
	free(rb->storage);
	free(rb);
}

/* Power-of-two capacity turns the wrap into a mask instead of a division,
 * which showed up as 8% of the ingest profile when it was a modulo. */
static size_t mask(const struct ring_buffer *rb, size_t index)
{
	return index & (rb->capacity - 1);
}

size_t ring_buffer_available(const struct ring_buffer *rb)
{
	size_t head = atomic_load(&rb->head);
	size_t tail = atomic_load(&rb->tail);
	return rb->capacity - (head - tail);
}

/* Returns bytes written: 0 when the buffer is full. Never partially writes,
 * so a record is either wholly visible to the consumer or not at all. */
size_t ring_buffer_write(struct ring_buffer *rb, const uint8_t *data, size_t len)
{
	if (len == 0 || ring_buffer_available(rb) < len)
		return 0;
	size_t head = atomic_load_explicit(&rb->head, memory_order_relaxed);
	size_t offset = mask(rb, head);
	size_t first = rb->capacity - offset;
	if (first > len)
		first = len;
	memcpy(rb->storage + offset, data, first);
	memcpy(rb->storage, data + first, len - first);
	atomic_store_explicit(&rb->head, head + len, memory_order_release);
	return len;
}

size_t ring_buffer_read(struct ring_buffer *rb, uint8_t *out, size_t len)
{
	size_t head = atomic_load_explicit(&rb->head, memory_order_acquire);
	size_t tail = atomic_load_explicit(&rb->tail, memory_order_relaxed);
	size_t pending = head - tail;
	if (pending < len)
		len = pending;
	if (len == 0)
		return 0;
	size_t offset = mask(rb, tail);
	size_t first = rb->capacity - offset;
	if (first > len)
		first = len;
	memcpy(out, rb->storage + offset, first);
	memcpy(out + first, rb->storage, len - first);
	atomic_store_explicit(&rb->tail, tail + len, memory_order_release);
	return len;
}
