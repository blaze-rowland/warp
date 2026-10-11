P1000p%begin 1700000000 1 0
%end 1700000000 1 0
%config-error /etc/tmux.conf:3: unknown command: bogus
%window-add @0
%session-changed $0 main
%begin 1700000001 5 1
%pause %0
%end 1700000001 5 1
%extended-output %0 1500 : café\015\012
%continue %0
%begin 1700000002 6 1
bind-key -T root C-b send-prefix
%end 1700000002 6 1
%subscription-changed cmd $0 @0 0 %0 : vim
%unlinked-window-add @3
%unlinked-window-renamed @3 scratch
%client-session-changed /dev/ttys002 $1 other
%exit server exited
\