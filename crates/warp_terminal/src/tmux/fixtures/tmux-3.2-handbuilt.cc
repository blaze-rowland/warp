P1000p%begin 1600000000 1 0
%end 1600000000 1 0
%window-add @0
%sessions-changed
%session-changed $0 0
%output %0 $ 
%begin 1600000001 8 1
%end 1600000001 8 1
%window-pane-changed @0 %1
%layout-change @0 8205,80x24,0,0{40x24,0,0,0,39x24,41,0,1} 8205,80x24,0,0{40x24,0,0,0,39x24,41,0,1} *
%output %1 \033[1mhi\134\015\012
%begin 1600000002 9 1
%1 39
%end 1600000002 9 1
%client-detached /dev/ttys001
%exit
\$ 