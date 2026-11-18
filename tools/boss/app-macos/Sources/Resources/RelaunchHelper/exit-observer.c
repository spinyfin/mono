#include <sys/event.h>
#include <sys/wait.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

// Observe a LaunchServices child without changing how macOS launches the app.
// A bounded wait also covers processes that remain alive past the handshake.
int main(int argc, char **argv) {
    if (argc != 3) return 64;
    int queue = kqueue();
    if (queue < 0) return 1;
    struct kevent change, event;
    EV_SET(&change, atoi(argv[1]), EVFILT_PROC, EV_ADD | EV_ONESHOT,
           NOTE_EXIT | NOTE_EXITSTATUS, 0, NULL);
    struct timespec timeout = {atoi(argv[2]), 0};
    int result = kevent(queue, &change, 1, &event, 1, &timeout);
    if (result == 1 && !(event.flags & EV_ERROR)) {
        int status = (int)event.data;
        if (WIFEXITED(status)) printf("exitCode=%d\n", WEXITSTATUS(status));
        else if (WIFSIGNALED(status)) printf("signal=%d\n", WTERMSIG(status));
        else printf("waitStatus=%d\n", status);
    } else {
        printf("observerUnavailable=%d\n", result == 1 ? (int)event.data : errno);
    }
    close(queue);
    return 0;
}
