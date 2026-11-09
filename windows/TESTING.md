# Validação do pacote em Windows 11 x64

Faça em uma máquina/VM Windows limpa, sem Rust, Visual Studio, GStreamer ou
Cloudflared no PATH. Os testes de hardware precisam de uma sessão gráfica real.

- Instale usando uma conta sem administrador, em um caminho com espaços/acentos.
  Abra o atalho do menu Iniciar e confirme que não aparece terminal nem falta DLL.
- Inicie com áudio e link público. Abra o link em outro computador; confirme vídeo,
  cursor, áudio, tela cheia e estatísticas em Chrome/Edge e Firefox.
- Faça uma chamada no Discord enquanto toca som em outro app: o espectador deve
  ouvir o app e não a chamada. Repita abrindo o Discord depois de iniciar a transmissão.
  Fale no microfone e confirme que ele não entra. Adicione um app às exclusões.
- Abra/feche/reabra apps com áudio, mude de fones e deixe todos em silêncio.
  O vídeo deve continuar; o áudio de novos apps deve aparecer sem reiniciar.
- Teste dois monitores com resoluções diferentes e gráficos híbridos de notebook.
  Confirme que a seleção da janela corresponde ao monitor capturado.
- Teste NVIDIA, Intel e AMD disponíveis; force `--encoder x264enc` no terminal
  para verificar o fallback por CPU. Verifique logs se o encoder escolhido falhar.
- Feche/reabra o viewer; aguarde 10 segundos sem espectadores e confirme pausa.
  Reabra e confirme retorno de vídeo e som. Teste dois espectadores simultâneos.
- Com espectadores conectados, clique Parar; confirme que servidor e cloudflared
  encerram, a porta é liberada e é possível iniciar outra transmissão.
- Feche a janela: o ícone continua. Clique no ícone para reabrir; use Sair e
  confirme que nenhum processo auxiliar continua. Encerre a interface pelo Gerenciador
  de Tarefas e confirme que o Job Object encerra também servidor e túnel.
- Reinicie Explorer: o ícone deve reaparecer. Abra pelo menu Iniciar uma segunda
  vez e confirme que a janela existente aparece, sem outro servidor.
- Instale com início automático e reinicie o Windows: deve abrir apenas o ícone,
  sem transmissão. Desinstale e confirme a remoção dos atalhos e dependências privadas.
- Teste modo Tailscale conectado e desconectado. Sem Tailscale, o modo privado
  deve recusar iniciar. Verifique o comportamento do Firewall em uma rede privada.

Limitações compartilhadas com Linux: chamadas dentro do navegador não podem ser
separadas de outros sons do mesmo app; o túnel não fornece TURN para redes em que
a conexão WebRTC direta é bloqueada. Teste esses cenários sem prometer cobertura.
