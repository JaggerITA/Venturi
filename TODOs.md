UNDO STACK: 
- durante il trascinamento di uno slider o trascinamento del campo numerico dal pannello delle proprietà, ogni incremento/decremento viene inserito nello stack undo. se faccio un incremento di 10 punti, mi trovo a dover premere CTRL+z numerose volte per tornare indietro. Dovrebbe invece essere inserito un solo evento al rilascio del mouse. 

FINESTRA DI PREVIEW:
- implementare gli handle per posizione, scala e punto di ancoraggio per la clip selezionata direttamente in overlay sul riquadro video, così non è necessario modificare questi parametri dal pannello delle proprietà. Questo overlay con gli handle dovrebbe essere attivabile e disattivabile con un pulsante sotto al riquadro video.
- Quando Premo CTRL+f, il player deve andare a schermo intero. nella vista a schermo intero, compare un overlay in basso (solo al passaggio col mouse) con la barra di riproduzione (interagibile) e il tasto play/pausa - si esce con ESC.
