document.addEventListener('DOMContentLoaded', function() {
    // === Order Form Logic ===
    var orderForm = document.getElementById('order-form');
    var itemsContainer = document.getElementById('items-container');
    var addItemBtn = document.getElementById('add-item-btn');

    if (addItemBtn && itemsContainer) {
        addItemBtn.addEventListener('click', function() {
            var firstRow = itemsContainer.querySelector('.item-row');
            if (!firstRow) return;
            var newRow = firstRow.cloneNode(true);
            newRow.querySelector('select[name="flavor_id"]').selectedIndex = 0;
            newRow.querySelector('input[name="scoops"]').value = 1;
            var toppingSelect = newRow.querySelector('select[name="topping"]');
            if (toppingSelect) toppingSelect.selectedIndex = 0;

            var removeBtn = document.createElement('button');
            removeBtn.type = 'button';
            removeBtn.className = 'remove-item-btn';
            removeBtn.textContent = '\u00d7';
            removeBtn.onclick = function() { newRow.remove(); };
            newRow.appendChild(removeBtn);

            itemsContainer.appendChild(newRow);
        });
    }

    if (orderForm) {
        orderForm.addEventListener('submit', function(e) {
            e.preventDefault();
            var customerName = orderForm.querySelector('input[name="customer_name"]').value;
            var rows = itemsContainer.querySelectorAll('.item-row');
            var items = [];
            rows.forEach(function(row) {
                var flavorId = parseInt(row.querySelector('select[name="flavor_id"]').value);
                var scoops = parseInt(row.querySelector('input[name="scoops"]').value) || 1;
                var toppingEl = row.querySelector('select[name="topping"]');
                var topping = toppingEl && toppingEl.value ? toppingEl.value : null;
                items.push({ flavor_id: flavorId, scoops: scoops, topping: topping });
            });

            fetch('/api/orders', {
                method: 'POST',
                headers: {
                    'Content-Type': 'application/json',
                    'x-api-key': 'demo-key'
                },
                body: JSON.stringify({ customer_name: customerName, items: items })
            })
            .then(function(res) { return res.json(); })
            .then(function(data) {
                if (data.id) {
                    window.location.href = '/orders/' + data.id;
                } else {
                    alert('Error placing order: ' + JSON.stringify(data));
                }
            })
            .catch(function(err) { alert('Error: ' + err.message); });
        });
    }

    // === Order Status WebSocket ===
    var orderData = document.getElementById('order-data');
    if (orderData) {
        var orderId = orderData.dataset.orderId;
        var statusEl = document.getElementById('order-status');
        var wsStatusEl = document.getElementById('ws-status');
        var msgRef = 1;
        var heartbeatInterval = null;

        var statusEmojis = {
            pending: '\u23f3 Pending',
            preparing: '\ud83d\udc68\u200d\ud83c\udf73 Preparing',
            ready: '\u2705 Ready!',
            delivered: '\ud83c\udf89 Delivered!'
        };

        function updateStatus(status) {
            if (!statusEl) return;
            statusEl.textContent = statusEmojis[status] || status;
            statusEl.className = 'badge badge-large ' + status;

            var steps = document.querySelectorAll('.timeline-step');
            var statuses = ['pending', 'preparing', 'ready', 'delivered'];
            var idx = statuses.indexOf(status);
            steps.forEach(function(step, i) {
                if (i <= idx) {
                    step.classList.add('active');
                } else {
                    step.classList.remove('active');
                }
            });
        }

        function connect() {
            var protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
            var wsUrl = protocol + '//' + window.location.host + '/ws';
            var ws = new WebSocket(wsUrl);

            ws.onopen = function() {
                if (wsStatusEl) {
                    wsStatusEl.textContent = '\u2705 Connected to live updates';
                    wsStatusEl.className = 'ws-connected';
                }
                var joinMsg = [String(msgRef), String(msgRef), 'order:' + orderId, 'phx_join', { customer_name: 'Web User' }];
                ws.send(JSON.stringify(joinMsg));
                msgRef++;

                heartbeatInterval = setInterval(function() {
                    if (ws.readyState === WebSocket.OPEN) {
                        ws.send(JSON.stringify([null, String(msgRef), 'phoenix', 'heartbeat', {}]));
                        msgRef++;
                    }
                }, 30000);
            };

            ws.onmessage = function(event) {
                try {
                    var msg = JSON.parse(event.data);
                    var eventName = msg[3];
                    var payload = msg[4];

                    if (eventName === 'phx_reply' && payload && payload.response) {
                        if (payload.response.status) {
                            updateStatus(payload.response.status);
                        }
                    } else if (eventName === 'status_changed' && payload) {
                        updateStatus(payload.status);
                    }
                } catch (e) {
                    console.error('WS parse error:', e);
                }
            };

            ws.onclose = function() {
                if (wsStatusEl) {
                    wsStatusEl.textContent = '\ud83d\udd0c Disconnected - Reconnecting...';
                    wsStatusEl.className = 'ws-disconnected';
                }
                clearInterval(heartbeatInterval);
                setTimeout(connect, 3000);
            };

            ws.onerror = function() {
                if (wsStatusEl) {
                    wsStatusEl.textContent = '\u274c Connection error';
                    wsStatusEl.className = 'ws-error';
                }
            };
        }

        connect();
    }

    // === Support Chat (on About page) ===
    var chatMessages = document.getElementById('chat-messages');
    var chatInput = document.getElementById('chat-input');
    var chatSend = document.getElementById('chat-send');

    if (chatMessages && chatInput && chatSend) {
        var chatResponses = [
            { patterns: ['hour', 'open', 'close', 'when'], response: '\ud83d\udd50 We\'re open Mon-Wed 10am-9pm, Thu 10am-10pm, Fri 10am-11pm, Sat 9am-11pm, Sun 9am-8pm! Come visit us! \ud83c\udf1e' },
            { patterns: ['flavor', 'menu', 'what do you', 'options'], response: '\ud83c\udf66 We have 7 amazing tropical flavors! Coconut Dream, Pineapple Paradise, Passion Fruit Swirl, Guava Sunset, Mango Tango, Lychee Blossom, and Papaya Cream! Check out our <a href="/menu">full menu</a>! \ud83c\udf34' },
            { patterns: ['price', 'cost', 'how much', 'expensive'], response: '\ud83d\udcb0 Our scoops range from $4.00 to $5.25! Plus we have awesome specials like Mahalo Monday (buy 2 get 1 free!) and Aloha Hour (half-price 3-5pm daily)! \ud83c\udf89' },
            { patterns: ['special', 'deal', 'discount', 'promo'], response: '\ud83c\udf1f Current specials: \ud83c\udf1e Mahalo Monday - Buy 2, get 1 free! \ud83c\udf34 Tropical Thursday - 20% off tropical flavors! \u2615 Aloha Hour - Half-price single scoops 3-5pm daily!' },
            { patterns: ['topping', 'add', 'extra'], response: '\ud83e\uddc1 Toppings: Macadamia Nuts, Toasted Coconut, Mochi Bits, Li Hing Mui Powder, Hot Fudge, and Passion Fruit Drizzle! Prices range from $0.50 to $1.00 \ud83d\ude0b' },
            { patterns: ['order', 'buy', 'get some'], response: '\ud83d\uded2 Ready to order? Head over to our <a href="/order">order page</a> and we\'ll get scooping! \ud83c\udf68' },
            { patterns: ['location', 'where', 'address', 'find'], response: '\ud83d\udccd We\'re located in beautiful Honolulu, HI! \ud83c\udf3a Come feel the island vibes! \ud83c\udfd6\ufe0f' },
            { patterns: ['thank', 'thanks', 'mahalo'], response: '\ud83e\udd19 Mahalo to YOU! We appreciate your love for ice cream! Come back anytime! \ud83c\udf1a\u2728' },
            { patterns: ['hello', 'hi', 'hey', 'aloha', 'sup'], response: '\ud83c\udf3a Aloha friend! Welcome to Mahalo Ice Cream! What can I help you with today? \ud83c\udf66\u2728' },
            { patterns: ['best', 'recommend', 'favorite', 'popular'], response: '\ud83c\udfc6 Our most popular flavor is Coconut Dream - creamy coconut with toasted flakes! But honestly, you can\'t go wrong with any of them! \ud83e\udd24' },
            { patterns: ['vegan', 'dairy', 'allerg'], response: '\ud83c\udf31 Great question! Our Pineapple Paradise sorbet is dairy-free! Please let us know about any allergies when you order. Your safety is our priority! \u2764\ufe0f' },
        ];

        var defaultResponses = [
            '\ud83c\udf66 That\'s a great question! Feel free to ask about our flavors, hours, specials, or anything else! \ud83c\udf1e',
            '\ud83e\udd14 Hmm, I\'m not sure about that, but I DO know we have amazing ice cream! Ask me about our menu or specials! \ud83c\udf34',
            '\ud83c\udf1a I love your enthusiasm! Try asking about our flavors, toppings, prices, or store hours! \ud83c\udf66',
            '\ud83e\udd19 Mahalo for chatting! I can help with info about our flavors, specials, hours, and more! What would you like to know? \u2728',
        ];

        function addMessage(text, isUser) {
            var msgDiv = document.createElement('div');
            msgDiv.className = 'chat-message ' + (isUser ? 'user' : 'bot');

            var avatar = document.createElement('span');
            avatar.className = 'chat-avatar';
            avatar.textContent = isUser ? '\ud83d\ude0a' : '\ud83c\udf66';

            var bubble = document.createElement('div');
            bubble.className = 'chat-bubble';
            bubble.innerHTML = text;

            msgDiv.appendChild(avatar);
            msgDiv.appendChild(bubble);
            chatMessages.appendChild(msgDiv);
            chatMessages.scrollTop = chatMessages.scrollHeight;
        }

        function addTypingIndicator() {
            var msgDiv = document.createElement('div');
            msgDiv.className = 'chat-message bot';
            msgDiv.id = 'typing-indicator';

            var avatar = document.createElement('span');
            avatar.className = 'chat-avatar';
            avatar.textContent = '\ud83c\udf66';

            var bubble = document.createElement('div');
            bubble.className = 'chat-bubble typing';
            bubble.textContent = 'typing...';

            msgDiv.appendChild(avatar);
            msgDiv.appendChild(bubble);
            chatMessages.appendChild(msgDiv);
            chatMessages.scrollTop = chatMessages.scrollHeight;
        }

        function removeTypingIndicator() {
            var el = document.getElementById('typing-indicator');
            if (el) el.remove();
        }

        function getBotResponse(input) {
            var lower = input.toLowerCase();
            for (var i = 0; i < chatResponses.length; i++) {
                var entry = chatResponses[i];
                for (var j = 0; j < entry.patterns.length; j++) {
                    if (lower.indexOf(entry.patterns[j]) !== -1) {
                        return entry.response;
                    }
                }
            }
            return defaultResponses[Math.floor(Math.random() * defaultResponses.length)];
        }

        function sendMessage() {
            var text = chatInput.value.trim();
            if (!text) return;
            addMessage(text, true);
            chatInput.value = '';

            addTypingIndicator();
            var delay = 500 + Math.random() * 1000;
            setTimeout(function() {
                removeTypingIndicator();
                addMessage(getBotResponse(text), false);
            }, delay);
        }

        chatSend.addEventListener('click', sendMessage);
        chatInput.addEventListener('keydown', function(e) {
            if (e.key === 'Enter') sendMessage();
        });
    }

    // === Real-time Price Updates (menu & flavor detail pages) ===
    var priceElements = document.querySelectorAll('.price[data-flavor-id]');
    if (priceElements.length > 0) {
        var protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        var wsUrl = protocol + '//' + window.location.host + '/ws';
        var priceRef = 1;

        function connectPrices() {
            var ws = new WebSocket(wsUrl);

            ws.onopen = function() {
                var joinMsg = [String(priceRef), String(priceRef), 'store:lobby', 'phx_join', { customer_name: 'Price Watcher' }];
                ws.send(JSON.stringify(joinMsg));
                priceRef++;

                setInterval(function() {
                    if (ws.readyState === WebSocket.OPEN) {
                        ws.send(JSON.stringify([null, String(priceRef), 'phoenix', 'heartbeat', {}]));
                        priceRef++;
                    }
                }, 30000);
            };

            ws.onmessage = function(event) {
                try {
                    var msg = JSON.parse(event.data);
                    var eventName = msg[3];
                    var payload = msg[4];

                    if (eventName === 'price_updated' && payload) {
                        var flavorId = payload.flavor_id;
                        var newPrice = payload.price;
                        priceElements.forEach(function(el) {
                            if (el.dataset.flavorId === String(flavorId)) {
                                el.textContent = newPrice;
                                el.classList.add('price-updated');
                                setTimeout(function() { el.classList.remove('price-updated'); }, 600);
                            }
                        });

                        var toast = document.getElementById('price-update-toast');
                        if (toast) {
                            toast.textContent = '\ud83d\udcb0 ' + payload.flavor_name + ' is now ' + newPrice + '!';
                            toast.className = 'toast visible';
                            setTimeout(function() { toast.className = 'toast hidden'; }, 3000);
                        }
                    }
                } catch (e) {
                    console.error('Price WS parse error:', e);
                }
            };

            ws.onclose = function() {
                setTimeout(connectPrices, 5000);
            };
        }

        connectPrices();
    }
});
