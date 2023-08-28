### Installing

```
rustup install stable           # 1.72.0

cargo build

cargo run
```

### Testing

```
curl localhost:3000/testing -H "Authorization: Bearer 3333"
curl localhost:3000/testing -H "Authorization: Bearer 1pw"
# etc...
```

### Introduction


:wave: Hello, grateful to have worked on this take home assignment.


I wanted to write a bit about this submission and my process.


~:warning: First, I did not succeed in implementing a fully working solution in the time allocated to me. 
I feel quite bad about that. Specifically I was unable to rate-limit requests **per Bearer token**. I did
succeed in writing a working rate limiter per-endpoint as specified. Unfortunately
I was unable to get a working ratelimiter keyed on each token (explained below).~

:warning: Second, I was not able to write my implementation in the provided time limit. 
Attempting to fix my mistake took on the order of 5 hours
(a small amount of stubbornness may have been involved).
Along the way I learned a lot, so thank you for the exercise in discomfort!


### Overview


This solution primarily uses the `tower` crate to implement `Service`
for `TokenRateLimiter` which I wrote to limit API requests per unit time. 
The core primitive is to use a `tokio::sync::PollSemaphore` to handle permits
given out to concurrent requests and return a `RateLimitError` when those
permits have expired. The benefit of using a Semaphore is that when an
endpoint handler is called and returns, the permit is dropped, thereby
increasing the available requests for the endpoint by 1. This was a nice
primitive to use because of move semantics in Rust. By moving the permit into
the future associated with the wrapped handler, the semaphore would increase our
count of available permits at exactly the right time.


Specifically this implements a 'count/bucket' rate limiting strategy where each
endpoint (and ideally bearer token) has its own bucket of available requests. New
tokens are replenished when a request comes in at time `t` which is longer than 
`prev_t + rate_limit_duration` (we've waited long enough).

I chose to use `tower` because I was aware of the `Service` trait being used in both 
`axum` and `hyper` and knew that it was a powerful primitive for writing middleware.


While using `Service` to implement this solution I encountered several issues.

1. The only location where a service can make a decision based on the request is
   inside of `call()`. This is frustrating because it would be better to know if the
   the token+endpoint can handle more requests by calling `poll_ready()`. Because 
   `poll_ready()` has no access to the request body, it was not possible to
   inspect the HTTP Bearer token at that location.

2. A way around this might have been to move 'readiness' of the service into
   `call()` by communicating via `Err()`. Basically we can treat the service as
   always ready, but if we detect that there are no more remaining requests
   available for a given token, `return Err(...)` from `call()` instead. This is
   mentioned as an approach by in the 
   [axum::middleware documentation](https://docs.rs/axum/latest/axum/middleware/index.html#routing-to-servicesmiddleware-and-backpressure).
   However doing so would require that our underlying data for counting requests
   is `Send`.


👉 **Resolution**: Approach #2 was the best way to go about it. I stopped using `poll_ready()` 
and `PollSemaphore` and opted instead to only use `Arc<RwLock<HashMap<Token, Arc<Mutex<usize>>>>>`.
Within the `call` method I encoded the availability of the service as returning an
`Err(RateLimitError)`. This worked from the perspective of axum and tower.


~Additionally, choosing `Arc<RwLock<HashMap<Token, Arc<Mutex<usize>>>>` was a
mistake for several reasons~

1. [Still true] The overhead is large to say the least
2. [Not true] When attempting approach #2 above, `MutexGuard` is not `Send` for good reason.
   This would have worked fine for a single thread, but attempting to move it
   into a Boxed Future is impossible. An oversight on my part because of #2
   above.

👉 **Resolution**: Issue #2 with using `Arx<Mutex<usize>>` was actually related to
using `Mutex` across `await`. I was not aware of this as an issue, however a
quick search through the `tokio` documentation ([Holding a Mutex across an `.await`](https://tokio.rs/tokio/tutorial/shared-state#holding-a-mutexguard-across-an-await)) 
revealed that this is a common mistake (enough to document). It became clear
when I read:

"_This is because the compiler currently calculates whether a future is Send
based on scope information only. The compiler will hopefully be updated to
support explicitly dropping it in the future, but for now, you must explicitly
use a scope._"


### Alternative Approaches


Rust provides atomic data types via `std::sync::atomic`. I should have used them
from the onset. Using `AtomicU64` might have been a better choice because it is
cheaper than `Arc<Mutex<usize>>` and also has the necessary operations to count
reqs remaining.


I could have implemented shared state as `Arc<RwLock<HashMap<Token, AtomicU64>>` for
example.


👉 **Resolution**: I still think that working with atomics and a better implemented
concurrent `HashMap` may be a more performant and lower overhead approach to
this problem.


If I were to do this differently I might have avoided the overhead
of both `axum` and `tower` and used lower level HTTP libraries to simply grab
the bearer token off of requests directly and maintain my own shared state.

👉 **Resolution**: While both tower and axum are convenient, I can see that writing
a `tower::Service` may not be a two-hour scoped experience for me just yet.


### Conclusion


I would be happy to discuss this solution despite it not satisfying the
requirements. As a result of this exercise I have a deeper understanding of the
following:

* `tower` crate and the generic `Service` trait
* How to implement custom `Service`s and their associated types
* Pinned objects including futures (still working on pin projection)
* Polling futures directly and implementing custom futures (I threw mine away)
* How axum uses the `Service` trait as an architecture primitive (it's very
  clever)
* `tokio`'s `Instant`, `Semaphore` and `PollSemaphore`


I also feel that my implementation is not sufficient and stretched my Rust
knowledge to new places (learning is good), but did not allow me to complete within the time constraints.


:warning: After attempting this exercise, **I think I my Rust experience (at present) is insufficient for the role we are discussing.**


##### Resources Consulted

https://tokio.rs/blog/2021-05-14-inventing-the-service-trait

https://github.com/tower-rs/tower/blob/master/guides/building-a-middleware-from-scratch.md

https://tokio.rs/tokio/tutorial/shared-state#holding-a-mutexguard-across-an-await

https://docs.rs/tokio/latest/tokio/

https://docs.rs/tower/latest/tower/

https://docs.rs/axum/latest/axum/

