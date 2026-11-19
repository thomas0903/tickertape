// A small C++11 RAII wrapper over the tickertape C ABI (include/tickertape.h). Header-only; copy it into your
// project. It throws tickertape::Error for negative statuses and frees every handle in a destructor.
//
// SPDX-License-Identifier: MIT OR Apache-2.0
#ifndef TICKERTAPE_HPP
#define TICKERTAPE_HPP

#include "tickertape.h"

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <exception>
#include <memory>
#include <new>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <utility>

namespace tickertape {

// A failed call: the status code plus the library's message for it.
class Error : public std::runtime_error {
public:
    Error(tickertape_status status, const std::string &message)
        : std::runtime_error(std::string(tickertape_status_name(status)) + ": " + message), status_(status) {}
    tickertape_status status() const noexcept { return status_; }

private:
    tickertape_status status_;
};

// Throws on a negative status; returns TICKERTAPE_OK or TICKERTAPE_NONE.
inline tickertape_status check(tickertape_status status) {
    if (status < 0) throw Error(status, tickertape_last_error_message());
    return status;
}

namespace detail {
template <class T, void (*Free)(T *)> struct Deleter {
    void operator()(T *p) const noexcept { Free(p); }
};
inline void close_writer(tickertape_writer *w) { (void)tickertape_writer_close(w); }
inline void close_reader(tickertape_reader *r) { (void)tickertape_reader_close(r); }
inline int64_t to_ns(std::chrono::nanoseconds t) { return static_cast<int64_t>(t.count()); }
}  // namespace detail

class WriterOptions {
public:
    WriterOptions() : p_(tickertape_writer_options_new()) {
        if (!p_) throw std::bad_alloc();
    }
    WriterOptions &segment_size(uint64_t bytes) {
        check(tickertape_writer_options_set_segment_size(p_.get(), bytes));
        return *this;
    }
    WriterOptions &recovery_policy(int32_t policy) {
        check(tickertape_writer_options_set_recovery_policy(p_.get(), policy));
        return *this;
    }
    WriterOptions &notify(bool on) {
        check(tickertape_writer_options_set_notify(p_.get(), on ? 1 : 0));
        return *this;
    }
    WriterOptions &prefault(bool on) {
        check(tickertape_writer_options_set_prefault(p_.get(), on ? 1 : 0));
        return *this;
    }
    const tickertape_writer_options *get() const noexcept { return p_.get(); }

private:
    std::unique_ptr<tickertape_writer_options,
                    detail::Deleter<tickertape_writer_options, tickertape_writer_options_free>>
        p_;
};

class ReaderOptions {
public:
    ReaderOptions() : p_(tickertape_reader_options_new()) {
        if (!p_) throw std::bad_alloc();
    }
    // Return only durable records (covered by a checkpoint); see tickertape_reader_options_set_durable_only.
    ReaderOptions &durable_only(bool on) {
        check(tickertape_reader_options_set_durable_only(p_.get(), on ? 1 : 0));
        return *this;
    }
    ReaderOptions &notify(bool on) {
        check(tickertape_reader_options_set_notify(p_.get(), on ? 1 : 0));
        return *this;
    }
    ReaderOptions &busy_spin() {
        check(tickertape_reader_options_set_busy_spin(p_.get()));
        return *this;
    }
    const tickertape_reader_options *get() const noexcept { return p_.get(); }

private:
    std::unique_ptr<tickertape_reader_options,
                    detail::Deleter<tickertape_reader_options, tickertape_reader_options_free>>
        p_;
};

// The journal's single writer. Move-only; the destructor closes it without checkpointing (records stay visible
// but only checkpointed records are durable).
class Writer {
public:
    explicit Writer(const std::string &dir, const WriterOptions *options = nullptr) {
        tickertape_writer *w = nullptr;
        check(tickertape_writer_open(dir.c_str(), options ? options->get() : nullptr, &w));
        p_.reset(w);
    }

    // Appends a record; returns its sequence number. On return it is visible, not yet durable.
    uint64_t append(uint32_t schema_id, uint64_t venue_ts_ns, uint64_t local_ts_ns, const void *payload,
                    std::size_t len) {
        uint64_t seq = 0;
        check(tickertape_writer_append(p_.get(), schema_id, venue_ts_ns, local_ts_ns,
                                       static_cast<const uint8_t *>(payload), len, &seq));
        return seq;
    }

    // Appends a record whose payload `fill(uint8_t *buf, size_t len)` writes in place. If `fill` throws, nothing is
    // committed and the exception propagates (aborting costs a sync; see tickertape_writer_append_with).
    template <class Fill>
    uint64_t append_with(uint32_t schema_id, uint64_t venue_ts_ns, uint64_t local_ts_ns, std::size_t len,
                         Fill &&fill) {
        Context<typename std::remove_reference<Fill>::type> ctx{&fill, nullptr};
        uint64_t seq = 0;
        tickertape_status status = tickertape_writer_append_with(
            p_.get(), schema_id, venue_ts_ns, local_ts_ns, len,
            &trampoline<typename std::remove_reference<Fill>::type>, &ctx, &seq);
        if (ctx.error) std::rethrow_exception(ctx.error);
        check(status);
        return seq;
    }

    // Makes every record appended so far durable.
    void checkpoint() { check(tickertape_writer_checkpoint(p_.get())); }

    uint64_t next_seq() const {
        uint64_t seq = 0;
        check(tickertape_writer_next_seq(p_.get(), &seq));
        return seq;
    }

    tickertape_writer *get() const noexcept { return p_.get(); }

private:
    template <class F> struct Context {
        F *fill;
        std::exception_ptr error;
    };
    // Exceptions must not cross the C ABI: catch them here and rethrow after the call returns.
    template <class F> static int32_t trampoline(void *user_data, uint8_t *buf, std::size_t len) {
        Context<F> *ctx = static_cast<Context<F> *>(user_data);
        try {
            (*ctx->fill)(buf, len);
            return 0;
        } catch (...) {
            ctx->error = std::current_exception();
            return 1;
        }
    }
    std::unique_ptr<tickertape_writer, detail::Deleter<tickertape_writer, detail::close_writer>> p_;
};

// A reader. Records returned by next() are borrowed views, valid until the next call on this reader.
class Reader {
public:
    // The safe (pread) reader.
    explicit Reader(const std::string &dir, const ReaderOptions *options = nullptr) {
        tickertape_reader *r = nullptr;
        check(tickertape_reader_open(dir.c_str(), options ? options->get() : nullptr, &r));
        p_.reset(r);
    }

    // The zero-copy reader. Precondition: while it exists, no process other than a tickertape writer modifies the
    // journal's segment files (see tickertape_reader_open_mapped_unsafe).
    static Reader open_mapped_unsafe(const std::string &dir, const ReaderOptions *options = nullptr) {
        tickertape_reader *r = nullptr;
        check(tickertape_reader_open_mapped_unsafe(dir.c_str(), options ? options->get() : nullptr, &r));
        return Reader(r);
    }

    // Returns true with a record, false if none arrived within `timeout` (zero: don't wait; negative: forever).
    bool next(tickertape_record &out, std::chrono::nanoseconds timeout = std::chrono::nanoseconds(0)) {
        return check(tickertape_reader_next(p_.get(), detail::to_ns(timeout), &out)) == TICKERTAPE_OK;
    }

    void seek(uint64_t seq) { check(tickertape_reader_seek(p_.get(), seq)); }

    uint64_t position() const {
        uint64_t seq = 0;
        check(tickertape_reader_position(p_.get(), &seq));
        return seq;
    }

    tickertape_reader *get() const noexcept { return p_.get(); }

private:
    explicit Reader(tickertape_reader *r) : p_(r) {}
    std::unique_ptr<tickertape_reader, detail::Deleter<tickertape_reader, detail::close_reader>> p_;
};

}  // namespace tickertape

#endif  // TICKERTAPE_HPP
