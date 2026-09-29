#include <arpa/inet.h>
#include <errno.h>
#include <libproc.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/socket.h>

/*
 * Narrow Darwin libproc boundary for Rust. Return the owning requested pid,
 * zero for a complete negative observation, and -1 whenever the descriptor
 * table cannot be proved complete.
 *
 * A pid's table is listed and then each socket in it is read, so a
 * descriptor can close, or be reused for something that is not a socket,
 * between the two. The read then fails with EBADF or ENOTSOCK. That is not
 * an unreadable table, only a changed one: it is listed and read again, up
 * to CHANGED_TABLE_ATTEMPTS times, and only a read that finishes without a
 * change counts as an observation. A process that opens and closes sockets
 * while it is checked (the test binary's own threads, or a busy client) used
 * to fail closed here intermittently. With an instrumented build on
 * 2026-09-28, every such failure was one of these two errors (547 EBADF,
 * 1 ENOTSOCK).
 */
#define CHANGED_TABLE_ATTEMPTS 8

enum scan { SCAN_ABSENT = 0, SCAN_FOUND = 1, SCAN_FAILED = -1, SCAN_CHANGED = -2 };

static enum scan scan_pid(
  int32_t pid,
  int family,
  const void *local_address,
  uint16_t local_port,
  const void *foreign_address,
  uint16_t foreign_port
) {
  int bytes = proc_pidinfo(pid, PROC_PIDLISTFDS, 0, NULL, 0);
  if (bytes <= 0 || bytes % (int)sizeof(struct proc_fdinfo) != 0) return SCAN_FAILED;
  int capacity = bytes + 32 * (int)sizeof(struct proc_fdinfo);
  struct proc_fdinfo *fds = calloc(1, (size_t)capacity);
  if (fds == NULL) return SCAN_FAILED;
  int read_bytes = proc_pidinfo(pid, PROC_PIDLISTFDS, 0, fds, capacity);
  if (read_bytes >= capacity) {
    /* The table grew past the slack between the two listings. */
    free(fds);
    return SCAN_CHANGED;
  }
  if (read_bytes <= 0 || read_bytes % (int)sizeof(*fds) != 0) {
    free(fds);
    return SCAN_FAILED;
  }

  int count = read_bytes / (int)sizeof(*fds);
  for (int index = 0; index < count; index++) {
    if (fds[index].proc_fdtype != PROX_FDTYPE_SOCKET) continue;
    struct socket_fdinfo socket_info;
    errno = 0;
    int socket_bytes = proc_pidfdinfo(
      pid,
      fds[index].proc_fd,
      PROC_PIDFDSOCKETINFO,
      &socket_info,
      (int)sizeof(socket_info)
    );
    if (socket_bytes != (int)sizeof(socket_info)) {
      int error = errno;
      free(fds);
      return socket_bytes == 0 && (error == EBADF || error == ENOTSOCK) ? SCAN_CHANGED : SCAN_FAILED;
    }
    if (socket_info.psi.soi_kind != SOCKINFO_TCP ||
        socket_info.psi.soi_protocol != IPPROTO_TCP ||
        socket_info.psi.soi_family != family ||
        socket_info.psi.soi_proto.pri_tcp.tcpsi_state != TSI_S_ESTABLISHED) continue;

    const struct in_sockinfo *info = &socket_info.psi.soi_proto.pri_tcp.tcpsi_ini;
    if (ntohs((uint16_t)info->insi_lport) != local_port ||
        ntohs((uint16_t)info->insi_fport) != foreign_port) continue;
    int address_match = family == AF_INET
      ? memcmp(&info->insi_laddr.ina_46.i46a_addr4, local_address, sizeof(struct in_addr)) == 0 &&
        memcmp(&info->insi_faddr.ina_46.i46a_addr4, foreign_address, sizeof(struct in_addr)) == 0
      : memcmp(&info->insi_laddr.ina_6, local_address, sizeof(struct in6_addr)) == 0 &&
        memcmp(&info->insi_faddr.ina_6, foreign_address, sizeof(struct in6_addr)) == 0;
    if (address_match) {
      free(fds);
      return SCAN_FOUND;
    }
  }
  free(fds);
  return SCAN_ABSENT;
}

int32_t pty_inspect_socket_owner_darwin(
  const char *local_text,
  uint16_t local_port,
  const char *foreign_text,
  uint16_t foreign_port,
  const int32_t *pids,
  size_t pid_count
) {
  int family = strchr(local_text, ':') == NULL ? AF_INET : AF_INET6;
  if ((strchr(foreign_text, ':') == NULL ? AF_INET : AF_INET6) != family) return -1;

  struct in6_addr local6;
  struct in6_addr foreign6;
  struct in_addr local4;
  struct in_addr foreign4;
  void *local_address = family == AF_INET ? (void *)&local4 : (void *)&local6;
  void *foreign_address = family == AF_INET ? (void *)&foreign4 : (void *)&foreign6;
  if (inet_pton(family, local_text, local_address) != 1 ||
      inet_pton(family, foreign_text, foreign_address) != 1) return -1;

  for (size_t arg = 0; arg < pid_count; arg++) {
    int32_t pid = pids[arg];
    if (pid <= 0) return -1;
    enum scan result = SCAN_CHANGED;
    for (int attempt = 0; attempt < CHANGED_TABLE_ATTEMPTS && result == SCAN_CHANGED; attempt++) {
      result = scan_pid(pid, family, local_address, local_port, foreign_address, foreign_port);
    }
    if (result == SCAN_FOUND) return pid;
    if (result != SCAN_ABSENT) return -1;
  }
  return 0;
}
